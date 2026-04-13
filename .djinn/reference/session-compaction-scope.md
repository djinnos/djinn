---
title: Session Compaction Scope
type: reference
tags: ["scope","session-compaction","reference"]
---

# Session Compaction Scope

## In Scope

- Disable Goose's built-in auto-compaction by setting threshold to 0.0 at agent creation time
- Event-based token monitoring in `spawn_reply_task()` stream loop — check `tokens_in` after each agent turn against 80% of model's `context_window`
- Mid-run compaction: when threshold hit, gracefully cancel current turn, generate LLM summary from full Goose conversation history, start fresh Goose session with system prompt + summary
- Resume-time compaction: check tokens before dispatching a continuation — if paused session already over threshold, compact before resuming
- `continuation_of` nullable FK on `sessions` table pointing to previous session record ID
- New Djinn session record per compaction (old record finalized, new record created with `continuation_of`)
- Worktree preservation across compaction — same worktree, fresh context only
- Compaction prompt template (embedded, like other agent prompts) requesting detailed continuation summary
- Summary generation uses the session's own model via a separate Goose completion call
- SSE event for compaction (session_compacted or similar) so desktop knows a boundary occurred

## Out of Scope

- Goose-internal compaction modifications — we disable it, not patch it
- Dedicated cheaper model for summarization — use the session's model. Can revisit for cost optimization later
- Continuation cap / max compactions — not needed, stuck detection handles non-progress
- Tool output pruning (OpenCode-style pre-compaction cleanup) — our approach replaces the entire conversation, pruning is irrelevant
- UI/desktop implementation of session chain display — that's a desktop milestone, not server
- Adaptive thresholds (per-model or dynamic) — fixed 80% for now
- Compaction for task reviewers or epic reviewers — only worker and conflict resolver sessions compact (reviewers are short-lived)

## Preferences

- Keep compaction logic in the supervisor, not a new actor — it's part of session lifecycle management
- Reuse existing `tokens_for_session()` dual-path (SessionManager → Goose SQLite) for token reads
- Compaction prompt should follow the same `include_str!` embedded template pattern as other prompts
- The compaction summary call should NOT use extensions/tools — it's a pure text completion
- Log compaction events to the task activity log (event_type: "compaction") with token counts

## Relations

- [[roadmap]] — Post-V1 feature
- [[ADR-018: Djinn-Owned Session Compaction]] — Design decision driving this scope
- [[ADR-015: Session Continuity & Resume]] — Existing resume model being extended
- [[ADR-010: Session Cost Tracking — Per-Task Token Metrics]] — Token tracking infrastructure reused
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — Goose session infrastructure
- [[requirements/v1-requirements]] — Extends AGENT-19 (session persistence)
