---
title: ADR-055 Task-Branch Knowledge Promotion Flow
type: design
tags: ["adr-055","dolt","knowledge-branching","session-lifecycle","memory"]
---

# ADR-055 Task-Branch Knowledge Promotion Flow

Originated from task 019d8915-26da-7981-b4ab-6897e5abe7ff (`4hkv`).

## Purpose

Define the concrete integration contract for per-task knowledge branching so later Wave 2 implementation can wire task dispatch, session memory writes, extraction promotion, review, and cleanup without rediscovering the same lifecycle.

This note is intentionally grounded in the current SQLite/worktree implementation so the Dolt design reuses proven seams where possible and isolates new Dolt-specific logic to explicit hook points.

## Current baseline: what already exists

### 1. Dispatch already creates an isolated code worktree
- `server/crates/djinn-agent/src/actors/slot/worktree.rs`
  - `prepare_worktree(...)` creates or reuses git branch `task/{short_id}` and a matching `.djinn/worktrees/{short_id}` checkout.
  - This is the correct orchestration seam for knowledge-branch creation because it already owns task-scoped workspace setup and resume behavior.

### 2. Session records already persist the task worktree path
- `server/crates/djinn-db/src/repositories/session.rs`
  - `CreateSessionParams.worktree_path`
  - `SessionRepository::create(...)`
- `server/crates/djinn-agent/src/actors/slot/lifecycle.rs`
  - worker/reviewer lifecycle already creates a session after worktree preparation.
- This means task-scoped knowledge branch metadata can piggyback on the existing session/task lifecycle instead of inventing a second branch-selection channel.

### 3. MCP memory writes are already worktree-aware
- `server/src/server/mcp_handler.rs`
  - extracts `x-djinn-worktree-root`
- `server/crates/djinn-mcp/src/dispatch.rs`
  - `dispatch_tool_with_worktree(...)` routes memory writes/edits/deletes/moves with a worktree root
- `server/crates/djinn-agent/src/extension/handlers/memory_agent.rs`
  - `call_memory_write(...)` / `call_memory_edit(...)` pass the session worktree path into MCP
- `server/crates/djinn-mcp/src/tools/memory_tools/write_services.rs`
  - `note_repository(...)` builds `NoteRepository::with_worktree_root(...)`
- `server/crates/djinn-db/src/repositories/note/mod.rs`
  - `NoteRepository` already carries task-local write context via `worktree_root: Option<PathBuf>`

### 4. Current promotion pattern is worktree-file -> canonical DB/file sync
- `server/crates/djinn-db/src/repositories/note/crud.rs`
  - `sync_worktree_notes_to_canonical(...)` scans `.djinn/**/*.md` in a task worktree and upserts canonical note rows/files
- This is the closest existing analogue to post-task knowledge promotion.

### 5. Session extraction already identifies knowledge artifacts
- `server/crates/djinn-agent/src/actors/slot/session_extraction.rs`
  - extracts `notes_written_permalinks`
  - persists session taxonomy with `notes_written`
- `server/crates/djinn-agent/src/actors/slot/lifecycle.rs`
  - runs structural extraction for completed sessions
- This gives promotion review a ready-made source of candidate note IDs/permalinks.

## Proposed lifecycle contract under Dolt task branches

## Naming and ownership

### Branch names
- **Code branch**: keep existing git naming: `task/{task.short_id}`
- **Knowledge branch**: add Dolt naming derived from the stable task id, not session id:
  - `kb/task/{task.id}` or `task_{task.id}`
- Recommendation: use `kb/task/{task.id}` in Rust/domain code and centralize SQL-safe escaping in one adapter so branch naming is not reimplemented ad hoc.

### Ownership model
- Branch lifetime is **task-scoped**, not session-scoped.
- Multiple sessions for the same task reuse the same knowledge branch.
- Promotion/cleanup happens when the task reaches a terminal knowledge decision, not whenever a single session ends.

This matches the current code-worktree contract: resumed sessions reuse the same isolated state rather than forking again.

## End-to-end flow

### Phase A — task dispatch / branch bootstrap

#### Trigger
Worker dispatch path when a task is about to start or resume.

#### Existing hook
- `server/crates/djinn-agent/src/actors/slot/worktree.rs`
  - `prepare_worktree(...)`
- Secondary coordination seam:
  - `server/crates/djinn-agent/src/actors/slot/lifecycle.rs`

#### New Dolt-specific behavior
After code worktree preparation succeeds, ensure the knowledge branch exists:

1. Resolve canonical branch name for the project, initially `main`.
2. Ensure Dolt branch `kb/task/{task.id}` exists.
3. If absent, create it from canonical branch.
4. Record branch identity in session/task metadata for downstream consumers.

#### Proposed contract
Introduce a dedicated service seam, e.g.:

```rust
pub struct KnowledgeBranchRef {
    pub branch_name: String,
    pub base_branch: String,
    pub created: bool,
}

#[async_trait]
pub trait KnowledgeBranchManager {
    async fn ensure_task_branch(
        &self,
        project_id: &str,
        task_id: &str,
        base_branch: &str,
    ) -> anyhow::Result<KnowledgeBranchRef>;
}
```

#### Persistence
Extend session metadata so every session can answer “which knowledge branch am I on?” without inferring from worktree path:
- preferred: add `knowledge_branch` column to `sessions`
- acceptable interim scaffold: store in `sessions.metadata_json`

### Phase B — session-scoped reads and writes

#### Trigger
Any MCP memory mutation invoked from an agent session.

#### Existing hook chain
- `server/crates/djinn-agent/src/extension/handlers/memory_agent.rs`
- `server/src/server/mcp_handler.rs`
- `server/crates/djinn-mcp/src/dispatch.rs`
- `server/crates/djinn-mcp/src/tools/memory_tools/write_services.rs`
- `server/crates/djinn-db/src/repositories/note/mod.rs`

#### Reuse from current implementation
The current flow already routes task-local writes through a context-bearing repository (`with_worktree_root(...)`).
Under Dolt, the same pattern should become **branch-bearing repository context**.

#### Proposed contract
Replace “filesystem worktree root” as the write-routing primitive with a generalized knowledge context:

```rust
pub enum KnowledgeWriteTarget {
    Canonical,
    TaskBranch {
        task_id: String,
        branch_name: String,
        worktree_root: Option<PathBuf>,
    },
}
```

Then evolve `NoteRepository` construction from:
- `with_worktree_root(Some(path))`

toward:
- `with_knowledge_target(KnowledgeWriteTarget::TaskBranch { ... })`

#### Behavioral rules
1. **Reads during a task session** should resolve against the task branch so the agent sees:
   - all canonical notes inherited from `main`
   - all branch-local notes created/edited for that task
2. **Writes during a task session** should commit only to the task knowledge branch.
3. **Non-task contexts** (desktop/manual admin tools, planner tasks not bound to delivery work, maintenance flows) continue to use canonical branch unless explicitly branch-scoped.

#### Dolt-specific logic needed
Current SQLite reads/writes are against one DB plus optional mirrored markdown files. Dolt requires:
- a connection/session checkout pinned to the task branch, or
- repository methods that explicitly execute branch-qualified SQL through a branch manager.

Recommendation: do **not** spread `DOLT_CHECKOUT` calls across MCP handlers. Centralize branch selection inside a DB adapter/repository factory.

### Phase C — post-session extraction writes

#### Trigger
Completed or paused session runs structural extraction and then LLM extraction.

#### Existing hook
- `server/crates/djinn-agent/src/actors/slot/lifecycle.rs`
  - `run_structural_extraction(...)`
  - `run_llm_extraction(...)`
- `server/crates/djinn-agent/src/actors/slot/session_extraction.rs`
  - produces `notes_written_permalinks`

#### Contract
LLM extraction must continue to write candidate notes into the **same task knowledge branch** used during the session.
It must not bypass the branch and write directly to canonical memory.

#### Hook requirement
When `run_llm_extraction(...)` constructs or invokes memory-writing machinery, it must resolve the task's `knowledge_branch` from the session record and obtain a branch-scoped note repository.

#### Why this matters
Without this hook, direct tool-driven writes would be isolated but post-session extraction would still pollute canonical memory, defeating ADR-055.

### Phase D — promotion review when task work is ready

#### Trigger
Task reaches the current “has durable artifacts / ready for review / merge candidate” path.

#### Existing integration signals
- `server/crates/djinn-agent/src/actors/coordinator/dispatch.rs`
  - `simple_lifecycle_task_has_durable_artifacts(...)`
  - already considers `notes_written`
- `server/crates/djinn-agent/src/actors/slot/session_extraction.rs`
  - note permalinks written during sessions
- existing task-review / merge machinery in coordinator + `task_merge`

#### New contract
Promotion review becomes a sibling to code review, not an automatic side effect of session close.

1. Gather candidate knowledge diff for the task branch relative to canonical.
2. Build a review payload containing:
   - added notes
   - edited notes
   - deleted notes (normally should be rare / likely disallowed initially)
   - session evidence (`notes_written_permalinks`, task context, extraction-quality counters)
3. Apply ADR-054 quality gate per changed note.
4. Produce per-note outcomes:
   - `promote_as_is`
   - `promote_with_edit` / `merge_into_existing`
   - `discard`
   - `needs_human_review` (optional future expansion)

#### Proposed service seam
```rust
pub struct KnowledgePromotionCandidate {
    pub task_id: String,
    pub branch_name: String,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

#[async_trait]
pub trait KnowledgePromotionService {
    async fn diff_task_branch(&self, task_id: &str) -> anyhow::Result<KnowledgePromotionCandidate>;
    async fn apply_review(
        &self,
        task_id: &str,
        decisions: KnowledgePromotionDecisions,
    ) -> anyhow::Result<KnowledgePromotionResult>;
}
```

### Phase E — selective promotion to canonical

#### Reuse from current implementation
Current `sync_worktree_notes_to_canonical(...)` already embodies the concept “promote only note artifacts discovered in task-local space into canonical storage.”
That method can serve as the behavioral template, but not the final mechanism.

#### Dolt-specific logic
Dolt promotion must be diff/merge-based rather than filesystem scan based.

Recommended order:
1. Compute task-branch diff vs canonical branch.
2. Materialize approved note mutations.
3. Apply promotion onto canonical branch.
4. Create a single promotion commit per task review outcome.

#### Why not reuse `sync_worktree_notes_to_canonical(...)` directly?
Because that method assumes:
- source of truth is markdown under `.djinn/` in a git worktree
- canonical target is a single SQLite/file namespace

Under Dolt, the source of truth becomes relational rows on a Dolt branch. Markdown files may remain a mirror/export representation, but they are no longer the authoritative branching mechanism.

### Phase F — cleanup

#### Trigger
After promotion decision is finalized or task is abandoned/closed without promotion.

#### Existing analogous cleanup seam
- `server/crates/djinn-agent/src/actors/slot/worktree.rs`
  - task worktree reuse/teardown lifecycle
- coordinator merge/close flows already own end-of-task cleanup decisions

#### Contract
- If promoted: delete `kb/task/{task.id}` after merge/cherry-pick succeeds.
- If discarded/abandoned: delete `kb/task/{task.id}` without promotion.
- If task remains reopenable after failed review: keep the branch.

#### Proposed hook locations
- code-merge success path in task merge/post-review completion flow
- explicit task close/abandon transition handlers
- orphan recovery / purge job for branches belonging to closed tasks

## Integration points to implement later

## 1. Dispatch/session start

### Reuse
- `prepare_worktree(...)` remains the outer lifecycle entrypoint.
- `SessionRepository::create(...)` remains the place where per-session routing metadata is persisted.

### New Dolt logic
- create or reuse Dolt task knowledge branch
- store `knowledge_branch` on session record
- optionally cache branch info on task for quick lookup

## 2. Memory tool routing

### Reuse
- `x-djinn-worktree-root` request plumbing
- `dispatch_tool_with_worktree(...)`
- task-session aware memory handler entrypoints

### New Dolt logic
- add branch-aware repository factory/context propagation
- ensure reads, edits, moves, deletes all execute against task branch
- separate branch selection from filesystem mirroring

## 3. Session extraction + LLM extraction

### Reuse
- `notes_written_permalinks`
- session taxonomy counters
- background extraction launch from lifecycle teardown

### New Dolt logic
- extraction writer must resolve and use task knowledge branch
- promotion candidate assembly should prefer recorded note permalinks over re-scanning whole DB history

## 4. Promotion review and merge

### Reuse
- existing review orchestration concept: work is reviewed before landing
- coordinator durable-artifact detection already treats knowledge artifacts as merge-worthy outputs

### New Dolt logic
- branch diffing
- note-level promotion decisions
- merge/cherry-pick into canonical branch
- promotion commit metadata

## 5. Cleanup and maintenance

### Reuse
- worktree cleanup patterns from task lifecycle
- periodic maintenance jobs already exist in coordinator/server runtime

### New Dolt logic
- branch deletion in Dolt
- stale knowledge-branch sweeps for closed tasks
- Qdrant branch-payload cleanup for discarded branch-local embeddings

## Canonical contract by component

## Coordinator / slot lifecycle
- **Input**: task selected for dispatch
- **Must do**:
  1. prepare git worktree
  2. ensure Dolt knowledge branch
  3. create session with `worktree_path` + `knowledge_branch`
- **Must not do**: implicitly merge/promote knowledge on session end

## MCP memory tools
- **Input**: session-scoped request, currently carrying worktree root
- **Must do**:
  1. resolve task/session knowledge context
  2. use branch-scoped repository
  3. return canonical permalink/note identity regardless of branch
- **Must not do**: write canonical branch directly during task sessions

## Session extraction
- **Input**: completed session transcript
- **Must do**:
  1. record note permalinks written
  2. persist extraction quality metadata
  3. pass branch context to any post-session extracted writes
- **Must not do**: decide promotion/cleanup on its own

## Promotion reviewer
- **Input**: task id + branch diff + extraction evidence
- **Must do**:
  1. evaluate changed notes using ADR-054 gate
  2. produce explicit dispositions
  3. merge only approved changes
- **Must not do**: blindly merge entire task branch by default

## Cleanup
- **Input**: terminal task outcome and promotion result
- **Must do**:
  1. delete task branch on successful promotion or discard
  2. retain branch only when task is expected to resume
  3. delete branch-local vectors/embeddings for discarded notes
- **Must not do**: delete branch before promotion result is durable

## Recommended implementation order

1. **Introduce metadata seam first**
   - add `knowledge_branch` to session persistence / APIs
2. **Introduce branch-aware repository context second**
   - generalize `with_worktree_root(...)` into branch-aware memory routing
3. **Wire branch creation on dispatch third**
   - use `prepare_worktree(...)` lifecycle seam
4. **Wire LLM extraction writes fourth**
   - ensure all extraction paths obey branch routing
5. **Implement promotion diff/review/cleanup last**
   - once branch-scoped writes are reliable

## Explicit reuse vs new Dolt work

## Reuse current worktree/canonical sync patterns
These behaviors should carry forward conceptually:
- task-scoped isolation should be created at dispatch time
- resumed sessions should reuse prior isolated task state
- session records should carry the routing metadata needed by downstream tools
- memory tools should accept session-scoped context rather than infer global mode
- promotion should be a separate explicit lifecycle step after task work is reviewed
- cleanup should happen only after terminal task decisions

## Requires new Dolt-specific logic
These cannot be satisfied by the current SQLite/worktree pattern:
- true branch creation and deletion in the database
- branch-aware SQL connections or repository factories
- diffing note state between canonical and task branches
- selective merge/cherry-pick of approved knowledge changes
- branch-aware vector payload updates/deletes in Qdrant
- recovery and orphan cleanup for task knowledge branches independent of git worktrees

## Non-goals for Wave 2 first pass
- multi-branch knowledge merges across several tasks at once
- human UI for line-by-line note diff review
- supporting note deletions from task branches automatically landing on canonical
- planner/architect speculative branches beyond task-scoped delivery branches

## Decision summary

Use the **existing task worktree lifecycle as the orchestration model**, but migrate the knowledge source of truth from “task-local markdown files later synced into canonical SQLite” to “task-scoped Dolt branch later selectively promoted into canonical Dolt main.”

In practice:
- **dispatch** creates/reuses both code worktree and knowledge branch
- **sessions** persist which knowledge branch they use
- **memory tools and LLM extraction** read/write against that branch
- **promotion review** diffs the branch and applies ADR-054 gating
- **cleanup** deletes the branch only after promote/discard decisions are durable
