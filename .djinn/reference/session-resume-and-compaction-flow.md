---
title: Session Resume and Compaction Flow
type: reference
tags: ["architecture","diagrams","sessions","compaction","resume"]
---

# Session Resume and Compaction Flow

How sessions are resumed from paused state and how context compaction works.

## Session Resume vs Fresh Decision

```mermaid
flowchart TD
    A[Task dispatched to slot] --> B[find_paused_session_record]
    B --> C{Paused session\nfound?}

    C -->|No| FRESH[Fresh Path]
    C -->|Yes| D{model_id\nmatches?}

    D -->|No| FRESH
    D -->|Yes| E{agent_type\nmatches?}

    E -->|No| FRESH
    E -->|Yes| F{Worktree path\nexists on disk?}

    F -->|No| G[Mark old session Interrupted]
    G --> FRESH
    F -->|Yes| RESUME[Resume Path]

    FRESH --> F1[prepare_worktree: create branch + worktree]
    F1 --> F2[Run setup commands]
    F2 --> F3[session_manager.create_session - new Goose session]
    F3 --> F4[SessionRepository::create - new Djinn record]

    RESUME --> R1[Reuse existing worktree]
    R1 --> R2[resume_context_for_task]
    R2 --> R3[session_repo.set_running - mark session running again]
    R3 --> R4[Use existing legacy Goose session ID + resume kickoff]

    subgraph Resume Context Priority
        direction TB
        RC1[1. Task reviewer feedback comment] --> RC2[2. Merge validation failure context]
        RC2 --> RC3[3. Merge conflict file list]
        RC3 --> RC4[4. Generic: 'previous submission needs revision']
    end

    R2 -.-> RC1
```

## Compaction Flow (Inline at 80% Context)

```mermaid
sequenceDiagram
    participant RL as run_reply_loop
    participant LC as Lifecycle Main Loop
    participant Goose as Goose SessionManager
    participant Prov as Summary Provider
    participant DB as SessionRepository
    participant SSE as DjinnEvent Bus

    RL-->>LC: CompactionSignal { session_id, tokens_in, context_window }

    LC->>Goose: get_session(old_session_id) - read messages + extension_data
    Goose-->>LC: messages[], extension_data, token counts

    LC->>DB: Update old record -> status=Compacted (no ended_at)
    LC->>SSE: SessionChanged event

    LC->>Prov: provider.complete(old_messages, compaction_system_prompt)
    Prov-->>LC: Summary text

    LC->>Goose: create_session(worktree_path, task_name)
    Goose-->>LC: new_session_id

    LC->>Goose: Carry over extension_data (todo state)

    LC->>DB: Create new record (continuation_of: old_record_id)
    LC->>SSE: SessionCreated event

    LC->>LC: Create new GooseAgent + Provider + Extensions
    LC->>LC: kickoff = summary + optional resume_context

    Note over LC: Continue main loop with new session
    LC->>RL: run_reply_loop(new_agent, new_session_id, kickoff_summary)

    Note over DB: Session chain: A -> B -> C via continuation_of field
```

## Session Continuation Chain

```mermaid
flowchart LR
    subgraph "Session Chain for one task"
        A["Session A\n(root)\ncontinuation_of: NULL\nstatus: compacted"] -->|"80% tokens"| B["Session B\ncontinuation_of: A.id\nstatus: compacted"]
        B -->|"80% tokens"| C["Session C\ncontinuation_of: B.id\nstatus: paused"]
    end

    subgraph "What happens at each compaction"
        direction TB
        S1["1. Read old session messages + extension_data"]
        S2["2. Mark old record: status=compacted"]
        S3["3. Generate summary via provider.complete()"]
        S4["4. Create new Goose session"]
        S5["5. Carry over extension_data (todo state)"]
        S6["6. Create new Djinn record with continuation_of"]
        S7["7. New agent + summary as kickoff"]
        S1 --> S2 --> S3 --> S4 --> S5 --> S6 --> S7
    end
```

## Session Record States

| State | When | Worktree | ended_at |
|-------|------|----------|----------|
| `running` | Active session | exists | NULL |
| `paused` | Worker DONE or pause signal | preserved | NULL |
| `completed` | Reviewer/epic session ends OK | cleaned up | set |
| `failed` | Session error | cleaned up | set |
| `interrupted` | Kill received | cleaned up | set |
| `compacted` | Inline compaction | preserved (reused by next) | NULL |

## Relations
- [[Task Dispatch and Slot Pool Flow]]
- [[Task Lifecycle and Session Flow]]
- [[decisions/adr-036-structured-session-finalization-finalize-tools-and-forced-tool-choice|ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice]]
- [[ADR-015: Session Continuity and Resume]]
