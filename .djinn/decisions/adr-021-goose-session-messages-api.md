---
title: ADR-021: Goose Session Messages API
type: adr
tags: ["adr","session","api","session-viewer"]
---

## Context
The desktop session viewer (see desktop [[decisions/adr-007-session-viewer-unified-chat-ui-for-task-execution|ADR-007: Session Viewer — Unified Chat UI for Task Execution]]) needs to display actual agent conversation content: LLM messages, tool calls, tool results, and subagent activity.

**Current state:**
- Djinn's `sessions` table stores **metadata only** (id, task_id, model_id, agent_type, status, tokens, timestamps, continuation_of)
- Actual conversation history lives in **Goose's SQLite database** (`~/.djinn/sessions/`), accessed via Goose's `SessionManager` library
- No MCP tool or API endpoint exposes session messages — `session_show` returns only metadata
- The `legacy Goose session ID` field on each session record is the key to look up messages in Goose's DB

**SSE infrastructure already exists but is underutilized:**
- Server already emits `session.started`, `session.completed`, `session.token_update`, `session.compacted` SSE events
- Desktop ignores all session SSE events — handler only processes task/epic/project
- This causes the existing bug where task cards can't display model ID or running duration for active sessions, requiring the desktop to maintain a custom `Task` type with manual `active_session` fields that never get populated
- MCP request-response is wrong for live sessions — you'd have to poll

## Decision
Dual approach: **SSE for live sessions, MCP tool for history.**

### 1. MCP Tool — `session_messages` (completed sessions)

For fetching historical session content after execution is done.

```
session_messages(id: String, project: String) -> SessionMessagesOutput
```

**Input:**
- `id`: Djinn session UUID
- `project`: Absolute project path

**Output:**
```json
{
  "session_id": "...",
  "legacy Goose session ID": "...",
  "agent_type": "worker",
  "messages": [
    {
      "role": "system" | "user" | "assistant",
      "content": [
        { "type": "text", "text": "..." },
        { "type": "tool_use", "id": "...", "name": "Bash", "input": {} },
        { "type": "tool_result", "tool_use_id": "...", "content": "..." }
      ],
      "timestamp": "2026-03-05T19:02:52Z"
    }
  ]
}
```

**Implementation:**
1. Look up Djinn session record by `id` to get `legacy Goose session ID`
2. Use `SessionManager::get_session(legacy Goose session ID)` to load the Goose session
3. Access `session.conversation.messages()` to get the message list
4. Serialize messages to JSON, preserving content block structure
5. Return with session metadata (agent_type) so the UI knows who's talking

No pagination initially — compaction keeps per-session size bounded.

### 2. SSE — `session.message` (live sessions)

For streaming message content to the desktop as agents work in real-time.

**New DjinnEvent variant:**
```rust
SessionMessage {
    session_id: String,
    task_id: String,
    agent_type: String,
    message: serde_json::Value,  // Raw Goose message content block
}
```

**SSE wire format:**
```
event: session.message
data: {"type":"session","action":"message","data":{
  "session_id":"...","task_id":"...","agent_type":"worker",
  "message":{"role":"assistant","content":[{"type":"text","text":"..."}]}
}}
```

**Emission point:** The Goose reply loop in `lifecycle.rs` already processes agent events turn-by-turn. After each complete turn (assistant message + tool results), emit a `SessionMessage` event on the broadcast channel.

**Existing SSE events already useful for live sessions (no changes needed):**
- `session.started` → new agent bubble header (has agent_type, model_id, task_id)
- `session.token_update` → live token counter per turn
- `session.compacted` → compaction divider
- `session.completed` / `session.interrupted` / `session.failed` → final status

**Desktop currently ignores all session SSE events.** The handler in `sseEventHandlers.ts` only processes task/epic/project. Handling session events will also fix the existing bug where task cards can't show model/duration for active sessions.

### Message Content Types

Both the MCP tool and SSE events return the same message format — Goose messages as-is without transformation:
- **Text blocks**: Agent reasoning and responses
- **Tool use blocks**: Tool name, input parameters, tool_use_id
- **Tool result blocks**: Output content, linked to tool_use_id
- **Thinking blocks**: If present (model-dependent)

### Security
- MCP tool: project-scoped access only
- SSE: session events already broadcast on the shared channel — desktop filters by project
- Both are read-only

## Consequences
**Positive:**
- Desktop can render full agent conversations for both live and completed sessions
- Continuation chains + MCP tool = complete execution history for any task
- SSE message events enable real-time session viewer without polling
- Handling existing session SSE events fixes task card model/duration display (no server changes needed for that)
- No changes to Goose's storage model — read-only access to existing data

**Negative:**
- Couples the server to Goose's internal message format (if Goose changes it, API breaks)
- SSE message events increase broadcast channel volume significantly during active sessions
- Large sessions may produce large MCP payloads — mitigated by compaction keeping sessions bounded
- Must handle missing Goose session files (deleted/corrupted)

## Future: Virtual Office View

The SSE session event stream is designed to support a future "virtual office" visualization (PixelHQ-style) where each active agent is represented as a character in a 2D office. The `session.message` events provide the real-time activity state needed to drive agent animations:

- `tool_use` with Edit/Write/Bash → agent is at their desk coding
- Text-only assistant turn → agent is thinking
- `agent_type: "task_reviewer"` → agent is in the review room
- Verification failed / agent looping → warning indicator
- `session.completed` → agent idle, waiting for next dispatch
- Status transition events → agent walking between rooms

This requires no changes to the SSE event format — just a frontend state machine that maps current session activity to visual state. All active sessions should be streamed to the desktop (not just the one being viewed), which is why we broadcast all session events on the shared channel rather than requiring per-session subscriptions.

## Relations
- [[decisions/adr-018-djinn-owned-session-compaction|ADR-018: Djinn-Owned Session Compaction]]
- [[decisions/adr-015-session-continuity-resume|ADR-015: Session Continuity & Resume]]
- [[decisions/adr-019-mcp-as-single-api-and-typed-tool-schemas|ADR-019: MCP as Single API and Typed Tool Schemas]]
- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]]