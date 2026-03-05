---
title: OS Shell Hardening Scope
type: reference
tags: ["scope","sandbox","shell","reference"]
---


# OS Shell Hardening Scope

## In Scope

- Replace `workspace_guard` with a `Sandbox` trait + platform backends (ADR-013)
- Linux backend: `landlock` crate v0.4.x — read everywhere, write to worktree + `/tmp` + `/var/tmp`
- macOS backend: `sandbox-exec` with dynamic per-invocation policy — same read/write rules
- Fallback: existing `workspace_guard` when Landlock unavailable (kernel < 5.13, WSL1); log warning at startup
- Thread `worktree_path: &Path` through `spawn_agent_loop` → `handle_event` → `dispatch_tool_call` → `call_shell` (ADR-017)
- Agent-provided `workdir` retained only as `current_dir` for the child process, not used for policy
- Remove `external_dir` from `ShellParams` and `tool_shell()` schema
- Add `prepare_epic_reviewer_worktree(project_dir, batch_id)` — detached HEAD worktree at `batch-{uuid}` (ADR-016)
- Branch on `agent_type == EpicReviewer` in supervisor dispatch to call the new prep function
- Cleanup of `batch-*` worktrees on session end (same lifecycle as task worktrees)
- Stale `batch-*` worktree detection in board reconciliation on startup
- Delete `src/agent/workspace_guard.rs` once `Sandbox` trait covers all cases

## Out of Scope

- Network isolation (seccomp-BPF, egress proxy) — deferred per ADR-013; most workflows need network
- VM/container isolation (Firecracker, gVisor, Docker) — rejected per ADR-013; requires out-of-process agents
- Windows support — `cmd.exe` shell path remains unchanged
- CI/CD test infrastructure for Landlock — deferred

## Preferences

- `Sandbox` trait in a new `src/agent/sandbox/` module: `mod.rs` (trait + fallback), `linux.rs` (Landlock), `macos.rs` (seatbelt)
- Landlock detection at startup (not per-call): check kernel version once, store selected backend in supervisor state or as a lazy static
- Keep `workspace_guard.rs` functions until `Sandbox` trait is fully wired; delete in the same PR

## Relations

- [[ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt]]
- [[ADR-016: EpicReviewer Detached HEAD Worktree]]
- [[ADR-017: Shell Sandbox Implementation — Worktree Injection and Landlock Crate]]
- [[ADR-011: Workspace-Guarded Shell for Goose Frontend Tools]]
