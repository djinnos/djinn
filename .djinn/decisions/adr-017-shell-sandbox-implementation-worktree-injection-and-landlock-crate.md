---
title: ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate
type: adr
tags: ["adr","sandbox","landlock","shell","implementation"]
---


# ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate

Status: Accepted
Date: 2026-03-04
Related: [[decisions/adr-013-os-level-shell-sandboxing-landlock-seatbelt|ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]

## Context

ADR-013 decided to replace the string-heuristic `workspace_guard` with
OS-level Landlock/Seatbelt sandboxing. Two implementation questions were left
open: how the worktree path reaches `call_shell` server-side, and which Rust
crate provides the Landlock API.

**Worktree path propagation.** `call_shell` currently extracts `workdir` from
the agent-supplied tool arguments. For sandbox policy, the allowed write root
must be a server-side truth — not a value the agent (or a prompt injection)
can influence. The supervisor already has the worktree path at dispatch time
(`prepare_worktree` / `prepare_epic_reviewer_worktree`).

Two options were evaluated:

- *AppState lookup*: store `session_id → worktree_path` in a `DashMap`,
  have `call_shell` look up by session ID. Requires threading session_id too;
  adds indirection for something already in scope.
- *Explicit parameter chain*: thread `worktree_path: &Path` through
  `spawn_agent_loop` → `handle_event` → `dispatch_tool_call` → `call_shell`.
  The value is already in scope at each call site.

**Landlock crate.** The `landlock` crate is the official reference Rust binding
maintained by the Linux Landlock LSM developers. v0.4.4 released November 2025,
208 GitHub stars. No other viable Rust landlock binding exists; the alternative
is raw `libc` syscalls, which would require reimplementing the crate.

## Decision

1. **Worktree path via explicit parameter chain.** Thread `worktree_path: PathBuf`
   from `spawn_agent_loop` into `handle_event`, through `dispatch_tool_call`,
   and into `call_shell`. The agent-provided `workdir` argument is kept only as
   the `current_dir` for the child process — it is not used for sandbox policy.
   `external_dir` is removed from `ShellParams` and the tool schema.

2. **Use the `landlock` crate** (v0.4.x) for Linux sandbox implementation.
   No raw syscalls. The `Sandbox` trait described in ADR-013 wraps this crate
   on Linux and `sandbox-exec` on macOS.

## Consequences

### Positive

- Sandbox policy is fully server-controlled; prompt injection cannot influence
  the allowed write root by passing a crafted `workdir`.
- Explicit parameter threading makes the data-flow visible in function
  signatures — no hidden global or shared state.
- `landlock` crate is maintained by LSM developers; tracks kernel API changes.
- Removing `external_dir` simplifies the tool schema and eliminates the
  escape hatch that ADR-011 acknowledged as a weakness.

### Negative

- Three function signatures change (`handle_event`, `dispatch_tool_call`,
  `call_shell`) — mechanical but touches supervisor and extension code.
- `landlock` crate is low-star (208) — mitigated by its authoritative origin.
  No alternative Rust binding exists anyway.

## Relations

- [[decisions/adr-013-os-level-shell-sandboxing-landlock-seatbelt|ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]
- [[decisions/adr-011-workspace-guarded-shell-for-goose-frontend-tools|ADR-011: Workspace-Guarded Shell for Goose Frontend Tools]]
- [[OS Shell Hardening Scope]]
