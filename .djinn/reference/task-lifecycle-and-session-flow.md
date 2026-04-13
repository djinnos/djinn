---
title: Task Lifecycle and Session Flow
type: reference
tags: ["architecture","diagrams","lifecycle","sessions"]
---

# Task Lifecycle and Session Flow

What happens inside a slot after dispatch, from load to completion.

## High-Level Lifecycle

```mermaid
flowchart TD
    A[run_task_lifecycle] --> B{Kill/Pause\nalready signaled?}
    B -->|Yes| Z1[Return Free/Killed]
    B -->|No| C[Load Task]
    C --> D[Determine Agent Type]
    D --> E[Transition: in_progress / in_task_review]
    E --> F[Setup Provider Credentials]
    F --> G{Paused session\nexists?}

    G -->|Yes, matching model+type+worktree| H[Resume Path]
    G -->|No or mismatch| I[Fresh Path]

    H --> J[Reuse worktree + legacy Goose session ID]
    I --> K[prepare_worktree]
    K --> L[Run Setup Commands]

    J --> M[Create/Resume Goose Session]
    L --> M

    M --> N[Create GooseAgent + Provider + Extensions]
    N --> O[Render System Prompt]
    O --> P[Main Loop: Reply + Compaction + Verification]

    P --> Q{Outcome}
    Q -->|Worker DONE| R[Commit work, transition needs_task_review]
    Q -->|Reviewer VERIFIED| S[Squash-merge, close task]
    Q -->|Reviewer REOPEN| T[Release back to open]
    Q -->|Epic Clean| U[Mark batch clean]
    Q -->|Paused| V[Commit WIP, preserve worktree]
    Q -->|Killed| W[Commit WIP, cleanup worktree]

    R --> X[Emit SlotEvent::Free]
    S --> X
    T --> X
    U --> X
    V --> X
    W --> Y[Emit SlotEvent::Killed]

    X --> ZZ[Pool: free slot, trigger redispatch]
    Y --> ZZ
```

## Pause vs Kill

```mermaid
flowchart TD
    subgraph "Pause (CancellationToken: pause)"
        P1[pause token cancelled] --> P2[Get final token counts]
        P2 --> P3["session_repo.pause() - status='paused', no ended_at"]
        P3 --> P4[commit_wip_if_needed - WIP commit]
        P4 --> P5[Worktree PRESERVED]
        P5 --> P6["Emit SlotEvent::Free"]
        P6 --> P7[Slot returned to pool]
        P7 --> P8["Next dispatch: find_paused_session -> RESUME"]
    end

    subgraph "Kill (CancellationToken: cancel)"
        K1[cancel token cancelled] --> K2[Get final token counts]
        K2 --> K3["session_repo.update() - status='interrupted', ended_at set"]
        K3 --> K4[commit_wip_if_needed - WIP commit]
        K4 --> K5[cleanup_worktree - REMOVED]
        K5 --> K6["transition_interrupted() - task back to 'open'"]
        K6 --> K7["Emit SlotEvent::Killed"]
        K7 --> K8[Slot returned to pool]
        K8 --> K9["Next dispatch: no paused session -> FRESH"]
    end
```

## Full Worker -> Review -> Merge Cycle

```mermaid
sequenceDiagram
    participant W as Worker Agent
    participant LC1 as Worker Lifecycle
    participant DB as TaskRepository
    participant Coord as Coordinator
    participant LC2 as Reviewer Lifecycle
    participant R as Reviewer Agent
    participant Git as GitActor

    W->>LC1: WORKER_RESULT: DONE
    LC1->>LC1: Run setup + verification commands
    Note over LC1: All pass

    LC1->>LC1: commit_final_work_if_needed()
    LC1->>DB: session status = Paused (worktree preserved)
    LC1->>DB: task.transition -> needs_task_review
    LC1->>LC1: Emit SlotEvent::Free

    Note over Coord: Reacts to task_updated event
    Coord->>Coord: dispatch_ready_tasks()
    Coord->>Coord: role = "task_reviewer"
    Coord->>LC2: dispatch(task_id, model_id)

    LC2->>LC2: agent_type = TaskReviewer
    LC2->>LC2: prepare_worktree (reuses worker's branch)
    LC2->>R: Prompt with acceptance criteria

    alt Reviewer approves
        R->>LC2: REVIEW_RESULT: VERIFIED
        LC2->>Git: squash_merge(target, task_branch)
        Git-->>LC2: Ok(commit_sha)
        LC2->>Git: delete_branch(task_branch)
        LC2->>DB: task.transition -> closed
        LC2->>LC2: cleanup_worktree
        LC2->>LC2: maybe_queue_epic_review_batch()
    end

    alt Reviewer reopens
        R->>LC2: REVIEW_RESULT: REOPEN + FEEDBACK: "missing error handling"
        LC2->>DB: task.transition -> open
        LC2->>LC2: interrupt_paused_worker_session(task_id)
        Note over DB: Kills worker's paused session
        LC2->>LC2: cleanup_worktree

        Note over Coord: Next dispatch cycle
        Coord->>LC1: dispatch(task_id, worker_model)
        LC1->>LC1: find_paused_session -> None (interrupted)
        LC1->>LC1: Fresh session, resume_context = reviewer feedback
        Note over LC1: Worker addresses feedback
    end
```

## Relations
- [[Task Dispatch and Slot Pool Flow]]
- [[Session Resume and Compaction Flow]]
- [[decisions/adr-036-structured-session-finalization-finalize-tools-and-forced-tool-choice|ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice]]
- [[Setup Verification and Merge Conflict Flow]]
