---
title: Setup Verification and Merge Conflict Flow
type: reference
tags: ["architecture","diagrams","setup","verification","merge","conflict","epic-review"]
---

# Setup, Verification, Merge Conflict, and Epic Review Flows

## Setup and Verification Commands

```mermaid
sequenceDiagram
    participant LC as Lifecycle
    participant Cmd as commands.rs
    participant Shell as Worktree Shell
    participant Agent as GooseAgent

    Note over LC: SETUP (fresh sessions only, skipped on resume)
    LC->>Cmd: run_setup_commands_checked(task_id, worktree_path)
    Cmd->>Cmd: Load project.setup_commands
    loop Each setup command
        Cmd->>Shell: Execute in worktree cwd
        Shell-->>Cmd: exit code + output
        alt Command fails
            Cmd-->>LC: Error feedback (truncated to 50 lines)
            LC->>LC: Release task (transition back to open)
            Note over LC: Abort lifecycle
        end
    end
    Cmd-->>LC: None (all passed)

    Note over LC: ... agent works ... emits WORKER_RESULT: DONE ...

    Note over LC: VERIFICATION (after DONE marker)
    LC->>Cmd: run_setup_commands_checked(task_id, worktree_path)
    LC->>Cmd: run_verification_commands(task_id, worktree_path)

    alt Any command fails
        Cmd-->>LC: Feedback string
        LC->>Agent: Send feedback as user message
        Note over LC: Continue main loop (agent fixes and retries)
    else All pass
        Note over LC: Break loop, proceed to post-session transitions
    end
```

## Verification Retry Loop

```mermaid
flowchart TD
    A[Agent emits WORKER_RESULT: DONE] --> B[Run setup commands]
    B --> C{All pass?}
    C -->|No| D[Send failure feedback to agent]
    D --> E[Continue reply loop - agent fixes]
    E --> A

    C -->|Yes| F[Run verification commands]
    F --> G{All pass?}
    G -->|No| D
    G -->|Yes| H[Commit work, transition to needs_task_review]
```

## Merge Conflict Flow

```mermaid
sequenceDiagram
    participant Rev as TaskReviewer Agent
    participant LC as Lifecycle
    participant Git as GitActor
    participant DB as TaskRepository
    participant Act as Activity Log
    participant Coord as Coordinator

    Rev->>LC: REVIEW_RESULT: VERIFIED
    LC->>LC: success_transition() -> TaskReviewApprove

    LC->>Git: squash_merge(base_branch, task_branch, message)

    alt Merge succeeds
        Git-->>LC: Ok(commit_sha)
        LC->>Git: delete_branch(task_branch)
        LC->>DB: Transition -> closed (with commit_sha)
        LC->>LC: maybe_queue_epic_review_batch()
    end

    alt GitError::MergeConflict
        Git-->>LC: Err(MergeConflict { files })
        LC->>LC: Build MergeConflictMetadata { files, base, target }
        LC->>DB: Transition -> TaskReviewRejectConflict
        LC->>Act: Log activity type="merge_conflict" with metadata
        Note over DB: Task reopened (status -> open)

        Note over Coord: Next dispatch cycle
        Coord->>Coord: dispatch_ready_tasks()
        Coord->>Coord: conflict_context_for_dispatch(task) -> Some(metadata)
        Coord->>Coord: agent_type = ConflictResolver
        Note over Coord: Dispatch to slot with ConflictResolver role
    end

    alt GitError::CommitRejected (merge validation)
        Git-->>LC: Err(CommitRejected { cmd, exit_code, stderr })
        LC->>LC: Build MergeValidationFailureMetadata
        LC->>DB: Transition -> TaskReviewRejectConflict
        LC->>Act: Log activity type="merge_validation_failed"
        Note over DB: Task reopened -> dispatched as Worker (not ConflictResolver)
    end
```

## Conflict Resolver Session Setup

```mermaid
flowchart TD
    A[Task dispatched - conflict_context exists] --> B[agent_type = ConflictResolver]
    B --> C[prepare_worktree - normal branch + worktree]
    C --> D[git fetch origin/target_branch]
    D --> E["git merge --no-commit target_branch"]

    E --> F{Merge result?}
    F -->|Clean merge| G[git merge --abort - no markers needed]
    F -->|Conflicts| H[Leave conflict markers staged in worktree]

    G --> I[Create Goose session]
    H --> I

    I --> J[Render conflict-resolver.md prompt]
    J --> K["Inject: conflict_files, merge_base_branch, merge_target_branch"]
    K --> L[Agent resolves conflicts, commits]
    L --> M[Agent emits WORKER_RESULT: DONE]
    M --> N[Run setup + verification commands]
    N --> O[Transition to needs_task_review]
```

## Epic Review Flow

```mermaid
sequenceDiagram
    participant Task as Task Transition
    participant DB as EpicReviewBatchRepo
    participant Epic as EpicRepository
    participant Coord as Coordinator
    participant Pool as SlotPool
    participant LC as Lifecycle
    participant Agent as EpicReviewer Agent

    Note over Task: Task closed (after merge)
    Task->>Task: maybe_queue_epic_review_batch()
    Task->>DB: list_unreviewed_closed_task_ids(epic_id)
    DB-->>Task: [task_a, task_b, task_c]

    alt Tasks to review
        Task->>Epic: mark_in_review(epic_id)
        Task->>DB: create_batch(project_id, epic_id, [task_a, task_b, task_c])
        Note over DB: Batch status = "queued"
    end

    Coord->>DB: list_queued_anchors(limit)
    DB-->>Coord: [QueuedBatchAnchor { batch_id, task_id=task_a }]

    Coord->>Pool: dispatch(task_a, project_path, epic_reviewer_model)
    Pool->>LC: run_task_lifecycle(task_a, ...)

    LC->>LC: active_epic_batch_for_task(task_a) -> Some(batch)
    LC->>LC: agent_type = EpicReviewer

    LC->>LC: prepare_epic_reviewer_worktree (detached at HEAD)
    LC->>DB: mark batch "in_review" with session_id

    LC->>Agent: Prompt with batch context (task list + SHAs)

    loop For each task in batch
        Agent->>Agent: git show merge_sha - inspect changes
        Agent->>Agent: Architecture + integration review
    end

    alt All clean
        Agent-->>LC: EPIC_REVIEW_RESULT: CLEAN
        LC->>DB: mark_clean(batch_id)
        LC->>LC: Check: all tasks in epic closed?
        alt Yes
            LC->>Epic: close(epic_id)
        else No
            Note over Epic: Stays in_review for next batch
        end
    end

    alt Issues found
        Agent-->>LC: EPIC_REVIEW_RESULT: ISSUES_FOUND
        LC->>DB: mark_issues_found(batch_id, reason)
        LC->>Epic: reopen(epic_id)
        Note over Epic: New batch created when next task closes
    end
```

## Relations
- [[Task Lifecycle and Session Flow]]
- [[decisions/adr-036-structured-session-finalization-finalize-tools-and-forced-tool-choice|ADR-036: Structured Session Finalization — Finalize Tools and Forced Tool Choice]]
- [[Task Dispatch and Slot Pool Flow]]
- [[decisions/adr-024-agent-role-redesign-pm-architect-and-approval-pipeline|ADR-024: Agent Role Redesign — PM, Architect, and Approval Pipeline]]
- [[decisions/adr-014-project-setup-verification-commands|ADR-014: Project Setup and Verification Commands]]
