---
title: Reply Loop Nudge and Marker System
type: reference
tags: ["architecture","diagrams","nudge","markers","structured-output"]
---

# Reply Loop, Nudge, and Marker System

How the agent streams responses, tracks tokens, parses markers, and gets nudged for missing output.

## Reply Loop Sequence

```mermaid
sequenceDiagram
    participant LC as Lifecycle
    participant RL as run_reply_loop
    participant Agent as GooseAgent
    participant Goose as Goose SessionManager
    participant SSE as DjinnEvent Bus

    LC->>RL: (agent, kickoff, cancel, pause, context_window)

    RL->>Agent: agent.reply(kickoff, max_turns=300)
    activate Agent

    loop Stream Events
        Agent-->>RL: MessageEvent (text / tool_use / tool_result)
        RL->>RL: Track assistant_fragments (max 12)
        RL->>RL: Track saw_any_tool_use flag
        RL->>SSE: SessionMessage event (for UI)

        RL->>Goose: Read token counts from session
        Goose-->>RL: tokens_in, tokens_out
        RL->>SSE: SessionTokenUpdate event

        alt tokens_in / context_window >= 0.80
            RL->>RL: Set compaction_signal
            Note over RL: Break stream immediately
        end

        alt cancel.is_cancelled()
            Note over RL: interrupted = "session cancelled"
            Note over RL: Break
        end

        alt pause.is_cancelled()
            Note over RL: interrupted = "supervisor shutting down"
            Note over RL: Break
        end
    end
    deactivate Agent

    alt Compaction signaled
        RL-->>LC: return (Ok, partial_output, Some(CompactionSignal))
        Note over RL: Skip all marker checks
    else Normal completion
        RL->>Goose: Fetch persisted last assistant message
        RL->>RL: output.ingest_text() - parse markers

        alt saw_tool_use AND missing_required_marker()
            Note over RL: POST-SESSION NUDGE
            RL->>Agent: Send nudge message (max_turns=3)
            Agent-->>RL: Nudge response stream
            RL->>Goose: Fetch persisted message again
            RL->>RL: Re-parse markers
        end

        alt Still missing marker
            RL-->>LC: return (Err("missing marker"), output, None)
        else Marker found
            RL-->>LC: return (Ok, output, None)
        end
    end
```

## Marker Types per Agent Role

```mermaid
flowchart LR
    subgraph Worker / ConflictResolver
        W1["WORKER_RESULT: DONE"]
        W2["WORKER_RESULT: PROGRESS: description"]
        WN["Nudge: 'Emit exactly one final marker now:\nWORKER_RESULT: DONE.'"]
    end

    subgraph TaskReviewer
        T1["REVIEW_RESULT: VERIFIED"]
        T2["REVIEW_RESULT: REOPEN"]
        T3["FEEDBACK: what is missing"]
        TN["Nudge: 'Emit exactly one final marker now:\nREVIEW_RESULT: VERIFIED | REOPEN | CANCEL.\nIf REOPEN/CANCEL, also emit FEEDBACK.'"]
    end

    subgraph EpicReviewer
        E1["EPIC_REVIEW_RESULT: CLEAN"]
        E2["EPIC_REVIEW_RESULT: ISSUES_FOUND"]
        EN["Nudge: 'Emit exactly one final marker now:\nEPIC_REVIEW_RESULT: CLEAN | ISSUES_FOUND.\nIf ISSUES_FOUND, include actionable findings\nand create follow-up tasks.'"]
    end
```

## Nudge Timing Detail

```mermaid
flowchart TD
    A[Agent stream ends normally] --> B[Fetch persisted last assistant message from Goose SQLite]
    B --> C["output.ingest_text() - parse all markers"]
    C --> D{saw_any_tool_use?}

    D -->|No| E[Likely provider error - return Err]
    D -->|Yes| F{missing_required_marker?}

    F -->|No| G[Marker found - return Ok + parsed output]
    F -->|Yes| H[Build role-specific nudge message]

    H --> I["Send nudge as user message (max_turns=3)"]
    I --> J[Stream nudge response]
    J --> K[Fetch persisted message again]
    K --> L["Re-parse markers via ingest_text()"]
    L --> M{Still missing?}

    M -->|No| G
    M -->|Yes| N[Log detailed warning with all state]
    N --> O["Return Err with reason (agent failure)"]
```

## Key Design Decisions

- **Markers are parsed from Goose SQLite** (persisted messages), NOT from streaming chunks — ensures reliability
- **Nudge is limited to 3 turns** to prevent infinite loops if agent is confused
- **Compaction skips marker checks** — an agent at 80% context hasn't finished, markers aren't expected
- **saw_any_tool_use** distinguishes provider errors (no tools at all) from agent failures (worked but forgot marker)
- **ADR-012** established this pattern for structured output nudging

## Relations
- [[Task Lifecycle and Session Flow]]
- [[Session Resume and Compaction Flow]]
- [[Setup Verification and Merge Conflict Flow]]
- [[ADR-012: Epic Review Batches and Structured Output Nudging]]
