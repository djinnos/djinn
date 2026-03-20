---
title: ADR-020: Structured Activity Events for Command Runs
type: adr
tags: ["adr","activity-log","commands","session-viewer"]
---

## Context

The server runs user-configured setup and verification commands during task execution (see [[ADR-014: Project Setup & Verification Commands]]). Currently:

- **Failures**: Logged as `event_type: "comment"` with `actor_role: "verification"` — the payload is unstructured text containing the failed command name, exit code, and truncated stdout/stderr
- **Successes**: Not logged at all — completely invisible in the activity timeline
- **Pre-session setup**: Failures cause task release with a reason in `status_changed`, but no per-command breakdown

The desktop session viewer needs to show which commands ran, whether they passed or failed, and their duration — even on success. The current unstructured comment format makes this impossible to render as a proper UI component.

## Decision

**Always log a structured `commands_run` activity event** after setup or verification commands complete, regardless of success or failure.

### New Activity Event

```
event_type: "commands_run"
actor_id: "system"
actor_role: "system"
payload: {
  "phase": "setup" | "verification",
  "success": true | false,
  "commands": [
    {
      "name": "Install JS dependencies",
      "command": "pnpm install --frozen-lockfile",
      "exit_code": 0,
      "duration_ms": 1200,
      "stdout": null,
      "stderr": null
    },
    {
      "name": "TypeScript check",
      "command": "pnpm tsc --noEmit",
      "exit_code": 1,
      "duration_ms": 2300,
      "stdout": "...(truncated to last 50 lines)...",
      "stderr": "error TS2345: ..."
    }
  ]
}
```

### Rules
- Logged for BOTH setup phases: pre-session setup (before agent starts) and post-DONE setup re-check
- Logged for verification commands after worker signals DONE
- `stdout`/`stderr` included only for failed commands (exit_code != 0) — keeps payloads small on success
- Output truncation: same last-50-lines rule as current implementation
- The existing failure `comment` events can be removed or kept alongside — the `commands_run` event is the canonical source for the UI

### Logging Points in Lifecycle

1. **Pre-session setup** (`lifecycle.rs` ~line 850): After `run_commands(&setup_specs, &worktree_path)` returns, log `commands_run` with `phase: "setup"`
2. **Post-DONE setup check** (`commands.rs` `run_setup_commands_checked`): After `run_commands` returns, log `commands_run` with `phase: "setup"`
3. **Post-DONE verification** (`commands.rs` `run_verification_commands`): After `run_commands` returns, log `commands_run` with `phase: "verification"`

### Impact on Existing Behavior
- The failure feedback string still gets sent to the agent as a user message (no change to agent behavior)
- The failure comment can optionally be kept for backward compatibility, or replaced by the structured event
- `task_activity_list` returns these events like any other — no new MCP tool needed

## Consequences

**Positive:**
- Desktop UI can render per-command rows with name, status icon, and duration
- Failed commands auto-expand with stdout/stderr; successes show as compact checkmarks
- Complete audit trail of every command run during task execution
- No new MCP tools needed — `task_activity_list` serves it

**Negative:**
- Slightly larger activity log payloads (one extra event per setup/verification run)
- stdout/stderr for failures is duplicated if we keep the old comment events alongside

## Relations
- [[ADR-014: Project Setup & Verification Commands]]
- [[ADR-009: Simplified Execution — No Phases, Direct Task Dispatch]]
- [[ADR-018: Djinn-Owned Session Compaction]]