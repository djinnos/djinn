---
title: Diagnose djinn-agent prompt snapshot drift blocking ADR-053 verification
type: tech_spike
tags: ["djinn-agent","snapshots","verification","adr-053"]
---

# Diagnose djinn-agent prompt snapshot drift blocking ADR-053 verification

Originated from task 019d8725-d9b5-7aa0-a4c4-84e428c19611 (`3ok1`). Objective: determine why unrelated ADR-053 tasks are failing `cd server && cargo test -p djinn-agent` by storing new prompt snapshots instead of validating task-scoped behavior, and recommend the right remediation path.

## Scope paths
- `server/crates/djinn-agent/src/prompts.rs`
- `server/crates/djinn-agent/src/extension/tool_defs.rs`
- `server/crates/djinn-agent/src/extension/shared_schemas.rs`
- `server/crates/djinn-agent/src/snapshots/`
- `server/crates/djinn-mcp/`

## Findings

### 1. The churn is real snapshot drift, but it is **expected tool-surface fallout**, not evidence of a task-local regression
The failing verification comments on `rpgb`, `2744`, and `wpe0` all show the same pattern: `cargo test -p djinn-agent` runs hundreds of tests successfully, then writes new prompt snapshots for:
- `djinn_agent__prompts__tests__reviewer_tools_section_snapshot.snap.new`
- `djinn_agent__prompts__tests__worker_tools_section_snapshot.snap.new`

That failure mode matches `djinn-agent`'s snapshot tests in `server/crates/djinn-agent/src/prompts.rs`, where the tool section is rendered dynamically from each role's current tool schemas via `format_tools_section(&schemas)` and asserted with `insta::assert_snapshot!`.

Because the tools section is generated from live role schemas rather than hand-maintained prompt text, any intentional MCP tool-surface change will churn these snapshots even when the feature task being verified did not touch `djinn-agent` directly.

### 2. The drift comes from **post-snapshot tool schema changes on main**
The key commits after the initial auto-generated prompt-snapshot work are:

- `20ecf9d2` — ADR-051 planner/architect prompt and tool-surface migration
- `70510abc` — `memory_write` gained optional `status?`, and agent prompt snapshots were regenerated
- `08414f6f` — worker/reviewer gained `memory_build_context`
- `857ec849` — `github_fetch_file` added and `github_search` description/schema wording changed

The current `HEAD` diff against `20ecf9d2` shows the worker snapshot changed at least because:
- `memory_write(content, title, type, tags?)` became `memory_write(content, title, type, status?, tags?)`

`tool_defs.rs` also now adds:
- `github_fetch_file` to the base tool surface
- `memory_build_context` to worker and reviewer roles
- updated `github_search` description text for the GitHub Code Search API
- `memory_move` to architect

Those are exactly the kinds of schema changes that should alter prompt snapshots.

### 3. This is not an ADR-053 feature regression signal
Nothing in the evidence points to filesystem browsing, OpenViking seam work, or djinn-db repair work accidentally corrupting prompt rendering logic. The failing snapshots are aligned with deliberate agent/MCP surface changes merged on main. The affected ADR-053 tasks are simply re-running a broad verifier that includes a load-bearing crate whose snapshots lag behind the branch baseline.

## Diagnosis

**Root cause:** snapshot expectations in `server/crates/djinn-agent/src/snapshots/` are stale relative to intentional tool-schema changes already made around agent/MCP evolution. The failures are caused by prompt-tool serialization drift, not by the ADR-053 tasks under verification.

**Expected vs regression:** expected fallout. The observed changes are consistent with intentional tool additions/signature updates, especially `memory_write(status?)`, `memory_build_context`, and `github_fetch_file` / `github_search` schema wording updates.

## Recommended remediation

### Immediate board guidance
For ADR-053 tasks such as `2744`, `wpe0`, and any similar unrelated worker branches:
1. **Do not treat these snapshot writes as evidence of a task-specific bug.**
2. **Do not bounce the feature task solely on this verifier** unless the branch actually modified `server/crates/djinn-agent` prompt/tool code.
3. **Narrow verification scope for unrelated tasks** so task-scoped work is not blocked by repository-baseline `djinn-agent` snapshot drift.

### Correct fix path
Use one of these two paths explicitly:

#### Preferred short-term path
- For tasks unrelated to `server/crates/djinn-agent`, remove or bypass the broad `cargo test -p djinn-agent` gate from their required verification set.
- Keep task verification scoped to the changed subsystem.

#### Baseline cleanup path
- Run a dedicated prompt-schema cleanup task on a fresh branch from current main.
- Regenerate and review the `djinn-agent` prompt snapshots together with the schema/tool-surface changes, then land them as a single baseline-sync commit.

### What not to do
- Do **not** ask each unrelated ADR-053 worker to refresh and carry these snapshots independently.
- Do **not** assume the right remedy is to repair prompt logic unless a future diff shows unexpected removals/reordering beyond the known schema changes above.

## Planner-facing conclusion
This is a **verification-noise / stale-baseline** problem. The Planner should either:
- narrow verifier scope on unrelated ADR-053 tasks, or
- queue one small baseline-maintenance task to refresh `djinn-agent` snapshots on main,

rather than looping feature workers through unrelated `insta` churn.

## Evidence referenced
- `server/crates/djinn-agent/src/prompts.rs` snapshot tests for role tool sections
- `server/crates/djinn-agent/src/extension/tool_defs.rs` current tool-surface definitions
- `server/crates/djinn-agent/src/extension/shared_schemas.rs` shared tool schemas
- verification comments on tasks `rpgb`, `2744`, and `wpe0`
- commit history: `40135719`, `20ecf9d2`, `70510abc`, `08414f6f`, `857ec849`

## Confidence
High. The observed drift lines up with known intentional schema/tool changes and not with the ADR-053 feature codepaths being verified.