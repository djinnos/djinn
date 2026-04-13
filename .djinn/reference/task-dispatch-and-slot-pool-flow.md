---
title: Task Dispatch and Slot Pool Flow
type: reference
tags: ["architecture","diagrams","dispatch","slots"]
---

# Task Dispatch and Slot Pool Flow

How the coordinator picks up ready tasks and assigns them to execution slots.

## Task Dispatch Sequence

```mermaid
sequenceDiagram
    participant Evt as Domain Event / 30s Tick
    participant Coord as CoordinatorActor
    participant DB as TaskRepository
    participant Health as ModelHealth
    participant Pool as SlotPool
    participant Slot as SlotActor

    Evt->>Coord: task_created / task_updated / tick
    Coord->>Coord: dispatch_ready_tasks()

    Coord->>DB: list_ready(dispatch_limit=50)
    DB-->>Coord: Vec<Task> (open + needs_task_review, no unresolved blockers)

    Coord->>Coord: Sort by priority ASC, then created_at ASC

    loop For each ready task
        Coord->>Coord: Skip if project paused/unhealthy
        Coord->>Pool: has_session(task_id)?
        Pool-->>Coord: false (no duplicate dispatch)
        Coord->>Coord: role_for_task_status() -> "worker" | "task_reviewer"
        Coord->>Coord: resolve_dispatch_models_for_role(role)

        loop For each model (priority order)
            Coord->>Health: is_available(model_id)?
            Health-->>Coord: true
            Coord->>Pool: dispatch(task_id, project_path, model_id)

            Pool->>Pool: Pop free slot from free_slots[model_id]
            alt No free slot
                Pool-->>Coord: Err(AtCapacity)
                Note over Coord: Try next model
            else Slot available
                Pool->>Slot: RunTask { task_id, project_path }
                Slot-->>Pool: Ok(())
                Pool->>Pool: Update task_to_slot, task_started, slot_states
                Pool-->>Coord: Ok(())
                Note over Coord: Break model loop, next task
            end
        end
    end
```

## Slot Pool State Machine

```mermaid
stateDiagram-v2
    [*] --> Free: Pool initialized

    Free --> Busy: RunTask command
    Busy --> Free: SlotEvent::Free (normal completion / pause)
    Busy --> Free: SlotEvent::Killed (task killed)

    Free --> Draining: Drain requested (idle)
    Busy --> BusyDraining: Drain requested (active)
    BusyDraining --> Draining: Task completes
    Draining --> [*]: Slot retired

    state Busy {
        [*] --> Running
        Running --> Pausing: pause token cancelled
        Running --> Killing: cancel token cancelled
        Pausing --> [*]: WIP commit + preserve worktree
        Killing --> [*]: WIP commit + cleanup worktree
    }
```

## Coordinator Main Loop

```mermaid
flowchart TD
    subgraph "CoordinatorActor::run() - tokio::select!"
        A["cancellation_token.cancelled()"] -->|shutdown| Z[Graceful exit]

        B["message_rx.recv()"] -->|API call| C[Handle MCP tool request]
        C --> D[dispatch_ready_tasks if needed]

        E["event_rx.recv()"] -->|domain event| F{Event type?}
        F -->|task_created / task_updated| D
        F -->|session_changed| G[Update metrics]
        F -->|project_changed| H[Refresh project state]

        I["30s interval.tick()"] -->|safety net| D

        D --> J[dispatch_ready_tasks]
        J --> K[Also: dispatch epic review batches]
    end
```

## Relations
- [[Task Lifecycle and Session Flow]]
- [[Session Resume and Compaction Flow]]
- [[decisions/adr-009-simplified-execution-—-no-phases,-direct-task-dispatch|ADR-009: Simplified Execution]]
