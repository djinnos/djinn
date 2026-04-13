---
title: ADR-018: Djinn-Owned Session Compaction
type: adr
tags: ["adr","session-compaction"]
---

# ADR-018: Djinn-Owned Session Compaction

## Status
Accepted

Date: 2026-03-04

## Context

Goose has built-in context compaction (80% threshold, progressive tool-response
filtering, LLM summarization) but it does not work reliably across all model
backends. A session was observed hitting 2.3M tokens against a 400K context
window — Goose's compaction clearly failed to fire or was ineffective.

Djinn already owns the session lifecycle (dispatch, pause, resume, kill) and
tracks `tokens_in` per session in its own DB. The model catalog provides
`context_window` for every model. Djinn is the natural owner of compaction.

The Djinn plugin for Claude Code takes an advisory approach — injecting
WARNING/CRITICAL messages at 35%/25% remaining context — but this is purely
informational and doesn't actually compact. OpenCode takes a more robust
approach: detect overflow, prune old tool outputs, LLM-summarize the full
history, and inject the summary as a new message that becomes the conversation
boundary.

## Decision

**Disable Goose's built-in compaction** by setting its auto-compact threshold
to 0.0 at agent creation. Djinn implements its own compaction with these
properties:

### Detection
- **Event-based, mid-run**: After each agent turn in the `spawn_reply_task()`
  stream loop, check accumulated `tokens_in` against the model's
  `context_window` from the catalog.
- **Fixed 80% threshold**: Trigger compaction when `tokens_in >= 0.8 *
  context_window`. No adaptive thresholds — simple and predictable.
- Also checked at resume-time before dispatching a continuation.

### Compaction Flow
1. Cancel the current Goose agent turn gracefully
2. Read the full Goose conversation history from the session
3. Send history to the **same model** with a compaction prompt requesting a
   detailed continuation summary (what was done, current state, files changed,
   what remains)
4. Create a **new Goose session** with: system prompt (same as fresh dispatch)
   + LLM-generated summary as the first user message
5. Agent continues in the **same worktree** — code changes preserved, only
   context window is fresh

### Session Continuity (Option C)
- Create a new `SessionRecord` in Djinn's DB with `continuation_of` pointing
  to the previous session record ID
- Old Goose session preserved immutably — full message history available for
  UI scrollback
- New Goose session starts clean with summary + system prompt
- Desktop UI groups sessions by `continuation_of` chain, showing compaction
  boundaries as visual dividers between segments
- User can expand previous segments to see full history

### No Continuation Cap
- No maximum on continuation count — sessions can compact as many times as
  needed. Non-progress is detected by existing stuck-detection mechanisms
  (task stale thresholds, board reconciliation), not by counting compactions.

### Summary Generation
- Uses the session's own model (no dedicated cheaper model)
- Input: full Goose conversation history from the compacted session
- Output: detailed continuation prompt covering what was done, current state,
  files being modified, what to do next
- The summary replaces the conversation — post-compaction context is ONLY
  system prompt + summary

## Consequences

**Positive:**
- Djinn has full control over when and how compaction happens — no reliance on
  Goose's internal behavior
- Event-based detection catches runaway sessions mid-execution, not just at
  resume boundaries
- Immutable session records preserve full history for UI while keeping agent
  context clean
- `continuation_of` chain gives desktop a natural way to display session
  segments with compaction boundaries
- Token tracking per session segment stays accurate for cost reporting
- Same worktree preservation means no code loss during compaction

**Negative:**
- Summary generation burns tokens (full history sent to LLM for summarization)
- If summary generation fails (model error, rate limit), need a fallback
  strategy
- Adds complexity to the supervisor's stream loop (mid-run token checking +
  compaction orchestration)
- Goose's built-in compaction disabled entirely — if Goose improves theirs
  later, we'd need to evaluate re-enabling

## Relations
- [[ADR-015: Session Continuity & Resume]] — Compaction extends the resume
  model with continuation chains
- [[ADR-010: Session Cost Tracking — Per-Task Token Metrics]] — Per-segment
  token tracking via continuation_of chain
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] —
  Goose provides the session infrastructure being wrapped
- [[Session Compaction Scope]] — Scope boundaries for implementation
- [[roadmap]] — Post-V1 feature
