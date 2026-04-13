---
title: ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt
type: adr
tags: ["security","sandboxing","landlock","seatbelt","shell"]
---

# ADR-013: OS-Level Shell Sandboxing — Landlock + Seatbelt

Status: Accepted
Date: 2026-03-04
Supersedes: [[ADR-011: Workspace-Guarded Shell for Goose Frontend Tools]]

## Context

ADR-011 introduced a workspace guard that validates shell command paths at the
application level. This is a string-parsing heuristic — it can be bypassed by
symlinks, indirect paths, or subprocesses that ignore the guard. The trade-off
section of ADR-011 explicitly acknowledged "it is not a kernel sandbox."

Industry practice (Claude Code, Codex CLI, Cursor) has converged on OS-level
enforcement for agent shell commands using platform-native primitives with
near-zero overhead:

- **Linux**: Landlock LSM (kernel 5.13+) for filesystem restrictions.
- **macOS**: Seatbelt (`sandbox-exec`) for filesystem policy.

Both enforce at the kernel level — child processes inherit the restrictions,
and no application-level bypass is possible.

Goose (our agent harness) has Seatbelt sandboxing in its Electron desktop app
but nothing in the library crate. The shell tool in the `goose` crate is a plain
`tokio::process::Command` with no OS-level wrapping. We must implement sandboxing
at our own `call_shell` layer.

Heavier isolation (Firecracker, gVisor, Docker) was considered but rejected:
Djinn agents run as in-process Goose tokio tasks. VM or container isolation would
require restructuring to out-of-process agent execution, adds 100-300ms startup
overhead, and is designed for untrusted multi-tenant platforms — not our threat
model. We trust the LLM provider and just need to contain accidental or
prompt-injected shell side-effects.

Network blocking via seccomp-BPF was considered but deferred. Most coding tasks
need network access for dependency resolution (`cargo build`, `npm install`),
so seccomp filters would be bypassed on the majority of invocations. Codex CLI
uses seccomp but pairs it with a binary toggle that most workflows set to
`network_access = true`. If network filtering becomes necessary, a proxy-based
approach (like Claude Code's egress proxy) is more flexible than seccomp's
binary on/off. Filesystem isolation is the high-value protection — preventing
writes to `~/.ssh/`, `~/.bashrc/`, and the main repo checkout.

## Decision

1. **Replace workspace_guard with OS-level sandboxing** on shell tool invocations.
   Filesystem-only enforcement; network is unrestricted. The `external_dir`
   parameter on the shell tool is removed — there is no application-level
   override when the kernel enforces the policy.

2. **Linux (default): Landlock.**
   - Landlock rules: allow write to active task worktree + `/tmp` + `/var/tmp`.
     Allow read to the entire filesystem.
   - No `CAP_SYS_ADMIN` or user namespace support required — works inside Docker
     and on VPS without `--privileged`.
   - No seccomp/network restrictions (deferred — see Context).

3. **macOS: Seatbelt.**
   - Dynamic policy generated per shell invocation.
   - Allow read to entire filesystem. Allow write to worktree + `/tmp`.
   - Network unrestricted.

4. **Fallback**: if the platform supports neither (kernel < 5.13, WSL1, exotic OS),
   fall back to the existing workspace_guard and log a warning at startup.

5. **Filesystem policy** (both platforms):
   - **Read**: everywhere (agents need to read system headers, toolchains, etc.)
   - **Write**: active task worktree path + `/tmp` + `/var/tmp`
   - No write to `~/.djinn/`, `~/.config/`, `~/.ssh/`, `.git/` of the main repo

6. **Implementation**: a `Sandbox` trait with platform-specific implementations.
   The `call_shell` function in `extension.rs` wraps command execution through
   the active sandbox backend. Detection order on Linux: Landlock availability
   check → fallback to workspace_guard.

## Consequences

### Positive

- Kernel-enforced filesystem isolation — no bypass via symlinks, subprocesses, or
  path tricks.
- Near-zero performance overhead (both Landlock and Seatbelt are process-level).
- Works inside Docker/VPS without `--privileged` (unlike bubblewrap/namespaces).
- Compatible with existing in-process Goose architecture (no process model change).
- Network unrestricted — no friction for `cargo build`, `npm install`, etc.
- Matches industry standard (Claude Code, Codex CLI, Cursor all use equivalent
  primitives for filesystem enforcement).

### Trade-offs

- Requires Linux kernel 5.13+ for Landlock (Ubuntu 22.04+, Debian 12+, Fedora 35+,
  Arch current). Older kernels fall back to application-level guard.
- macOS `sandbox-exec` is technically deprecated by Apple but remains functional
  and is used by Claude Code, Goose, and Cursor in production.
- No network isolation — a prompt-injected agent could exfiltrate data. Mitigated
  by filesystem restrictions (can't read `~/.ssh/` secrets to exfil). Network
  filtering via proxy is a future option if needed.

## Relations

- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]
- [[ADR-011: Workspace-Guarded Shell for Goose Frontend Tools]]
