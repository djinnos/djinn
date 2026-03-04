---
title: ADR-011: Workspace-Guarded Shell for Goose Frontend Tools
type: adr
tags: []
---

# ADR-011: Workspace-Guarded Shell for Goose Frontend Tools

Status: Superseded by [[ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]
Date: 2026-03-04

## Context

Worker/reviewer sessions run in per-task git worktrees, but tool execution allowed
commands and edits to target paths outside the active worktree. This could mutate
the user's root checkout and break task isolation.

We need predictable behavior across local development environments while keeping
shell capability available for common build/test workflows.

## Decision

1. Use Djinn frontend tools for task execution capabilities and remove Goose
   builtin developer tools from supervisor session extensions.
2. Add a dedicated workspace guard module to validate shell execution paths.
3. Expose a guarded internal `shell` frontend tool (not MCP-exposed to users)
   with these rules:
   - `workdir` is required.
   - Default allowlist: active task worktree and `/tmp` (optionally `/var/tmp`).
   - If command appears to target outside paths, return a structured
     `EXTERNAL_DIR_REQUIRED` error.
   - `external_dir=true` acts as explicit override when outside access is
     intentional.
4. Prompt guidance must instruct agents to run shell commands inside the active
   worktree and use `external_dir=true` only when intentional.

## Consequences

### Positive

- Prevents accidental writes to user root checkout by default.
- Preserves shell workflows (`pnpm`, `cargo`, tests) inside task worktrees.
- Provides explicit, auditable override path for exceptional operations.
- Keeps policy logic in Djinn server code (no Goose fork required).

### Trade-offs

- Path checking is policy-based and command-string heuristic; it is not a kernel
  sandbox.
- Some legitimate commands may require explicit `external_dir=true`.
- Guard logic adds complexity and should be covered by tests.
