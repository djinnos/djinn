---
title: Git Workflow Diagrams
type: reference
tags: ["git","workflow","diagrams","worktree","merge","architecture"]
---

# Git Workflow Diagrams

## 1. Architecture Overview — GitActor

The GitActor is a hand-rolled Ryhl-pattern actor: one per project repository, serializing all git operations through an mpsc channel.

```
┌─────────────────────────────────────────────────────────┐
│                      AppState                           │
│                                                         │
│   git_actors: HashMap<PathBuf, GitActorHandle>          │
│                                                         │
│   get_or_spawn(path) ─► if missing, spawn new actor     │
└──────────────┬──────────────────────────────────────────┘
               │
               ▼
┌──────────────────────────┐       mpsc(32)       ┌──────────────────────────┐
│    GitActorHandle        │ ───── GitMessage ───► │       GitActor           │
│    (cheap Clone)         │                       │    (tokio::spawn)        │
│                          │ ◄── oneshot Reply ─── │                          │
│  • current_branch()      │                       │  Hybrid approach:        │
│  • status()              │                       │  • Reads  → git2 crate   │
│  • head_commit()         │                       │  • Writes → git CLI      │
│  • create_branch()       │                       │    (tokio::process)      │
│  • create_worktree()     │                       │                          │
│  • remove_worktree()     │                       │  Holds: git2::Repository │
│  • squash_merge()        │                       │  + repo path             │
│  • delete_branch()       │                       │                          │
│  • rebase_with_retry()   │                       │                          │
└──────────────────────────┘                       └──────────────────────────┘
```

---

## 2. Task Dispatch — Worktree Preparation Flow

When a task is dispatched for execution, `prepare_worktree()` sets up the isolated working directory.

```mermaid
flowchart TD
    A[Task dispatched] --> B{Has paused session<br/>with existing worktree?}
    B -- Yes --> C[Reuse existing worktree<br/>.djinn/worktrees/SHORT_ID/]
    B -- No --> D[Remove stale worktree<br/>if leftover from crash]

    D --> E[ensure_target_branch_ready]
    E --> F{Target branch exists<br/>with commits?}

    F -- Yes --> G[Continue]
    F -- "Branch missing,<br/>HEAD exists" --> H[git branch TARGET HEAD]
    H --> G
    F -- "No commits at all<br/>(bare init)" --> I[git checkout -B TARGET<br/>git add .djinn/.gitignore<br/>git commit 'initialize']
    I --> G

    G --> J{task/SHORT_ID<br/>branch exists?}
    J -- No --> K[create_branch]
    J -- Yes --> L[try_rebase_existing_task_branch]

    K --> M[git fetch origin TARGET<br/>git branch task/SHORT_ID origin/TARGET<br/>fallback: local TARGET if no remote]

    L --> N[Rebase in temp sync worktree<br/>.djinn/worktrees/.sync-task-SHORT_ID/]
    N --> O{Rebase clean?}
    O -- Yes --> P[Branch updated]
    O -- No --> Q[Abort rebase, continue<br/>with existing branch as-is]

    M --> R[create_worktree<br/>git worktree add .djinn/worktrees/SHORT_ID/ task/SHORT_ID]
    P --> R
    Q --> R

    R --> S[Agent works in<br/>.djinn/worktrees/SHORT_ID/]
```

---

## 3. Repository Layout — Worktree Structure

```
project-root/                         ← main working tree (user's checkout)
├── .djinn/
│   ├── .gitignore                    ← ignores worktrees/, etc.
│   ├── notes/                        ← knowledge base (git-tracked)
│   └── worktrees/
│       ├── ab12/                     ← task worktree (task/ab12 branch)
│       │   ├── .git                  ← linked worktree metadata
│       │   ├── src/
│       │   └── ...
│       ├── cd34/                     ← another task worktree
│       ├── .sync-task-ab12/          ← ephemeral rebase worktree (cleaned up)
│       ├── .rebase-task-ab12-17.../  ← ephemeral merge-time rebase (cleaned up)
│       ├── .merge-main-17.../        ← ephemeral squash-merge worktree (cleaned up)
│       └── batch-B001/               ← epic review worktree (detached HEAD)
└── .git/
    └── worktrees/                    ← git's internal worktree tracking
        ├── ab12/
        └── cd34/
```

---

## 4. Squash Merge Flow (Post-Task Completion)

When a task completes successfully, its branch is squash-merged into the target branch.

```mermaid
flowchart TD
    A[Task completed] --> B[commit_final_work_if_needed<br/>git add -A + git commit --no-verify]

    B --> C[squash_merge called<br/>branch=task/SHORT_ID<br/>target=main]

    C --> D[Attempt 1 of 3<br/>retry loop for non-fast-forward]

    D --> E[git fetch origin main]

    E --> F[Pre-merge rebase<br/>in ephemeral worktree]
    F --> F1[git worktree add .rebase-task-SHORT_ID-TS task/SHORT_ID]
    F1 --> F2{git rebase origin/main}
    F2 -- OK --> F3[Task branch now up-to-date]
    F2 -- Fail --> F4[Abort rebase, continue<br/>squash will report real conflict]
    F3 --> F5[Remove rebase worktree]
    F4 --> F5

    F5 --> G[Create detached merge worktree<br/>git worktree add --detach<br/>.merge-main-TS origin/main]

    G --> H[git merge --squash task/SHORT_ID]
    H --> I{Merge result?}

    I -- Clean --> J[git diff --cached --name-only]
    I -- Conflict --> K[Collect unmerged files<br/>git merge --abort<br/>Return MergeConflict error]

    J --> J1{Any staged changes?}
    J1 -- No changes --> J2[Return current HEAD SHA<br/>no-op merge]
    J1 -- Has changes --> L[git commit -m MESSAGE]

    L --> M{Commit result?}
    M -- OK --> N[git rev-parse HEAD<br/>get commit SHA]
    M -- "Rejected<br/>(hooks failed)" --> O[Return CommitRejected error]

    N --> P[git push origin SHA:refs/heads/main]
    P --> R{Push result?}

    R -- OK --> S[Remove merge worktree<br/>Return MergeResult]
    R -- "Non-fast-forward<br/>(main moved)" --> T{Attempts left?}
    R -- "Transient error<br/>(lock, timeout)" --> U[Retry push<br/>up to 3x with jitter]

    T -- Yes --> D
    T -- No --> V[Return error]

    S --> W[delete_branch<br/>git branch -D task/SHORT_ID<br/>git push origin --delete task/SHORT_ID]
```

---

## 5. Branch Lifecycle — End to End

```mermaid
sequenceDiagram
    participant U as User/Board
    participant C as Coordinator
    participant S as Slot/Lifecycle
    participant G as GitActor
    participant R as Remote (origin)

    U->>C: Start task (short_id=ab12)
    C->>S: dispatch task

    Note over S,G: prepare_worktree()

    S->>G: fetch origin main
    G->>R: git fetch origin main
    R-->>G: latest refs

    S->>G: create_branch("ab12", "main")
    G->>G: git branch task/ab12 origin/main<br/>(fallback: local main)

    S->>G: create_worktree("ab12", "task/ab12")
    G->>G: git worktree add<br/>.djinn/worktrees/ab12/ task/ab12

    Note over S: Agent runs in worktree<br/>(Goose reply loop)
    S->>S: Agent makes commits<br/>in .djinn/worktrees/ab12/

    alt Task Paused
        S->>G: commit_wip_if_needed<br/>git add -A, commit --no-verify "WIP: interrupted"
        Note over S: Worktree preserved<br/>for resume
    else Task Completed
        S->>G: commit_final_work_if_needed

        Note over S,G: squash_merge()

        S->>G: fetch + rebase task branch
        S->>G: create detached merge worktree
        S->>G: git merge --squash task/ab12
        S->>G: git commit -m "feat: ..."
        S->>G: git push origin SHA:refs/heads/main
        G->>R: push squash commit

        S->>G: delete_branch("task/ab12")
        G->>G: git branch -D task/ab12
        G->>R: git push origin --delete task/ab12

        S->>G: remove_worktree(.djinn/worktrees/ab12/)
        G->>G: git worktree remove --force + prune
    else Task Killed/Failed
        S->>G: commit_wip_if_needed (best effort)
        S->>G: cleanup_worktree<br/>(remove unless paused session references it)
    end
```

---

## 6. Conflict Resolution Flow

```mermaid
flowchart TD
    A[squash_merge returns<br/>MergeConflict error] --> B[Task transitions to<br/>needs_review / reopened]

    B --> C[Task re-dispatched<br/>to agent]

    C --> D[prepare_worktree detects<br/>existing task/SHORT_ID branch]

    D --> E[try_rebase_existing_task_branch<br/>onto latest origin/main]

    E --> F{Rebase succeeds?}
    F -- Yes --> G[Agent continues work<br/>in fresh worktree]
    F -- No --> H[Agent gets worktree<br/>with unrebased branch]

    G --> I[Agent resolves any<br/>remaining issues]
    H --> I

    I --> J[Task completes again<br/>squash_merge retry]
```

---

## 7. Retry and Error Handling Summary

| Scenario | Strategy | Max Attempts | Backoff |
|---|---|---|---|
| **Push transient errors** (lock, timeout, connection) | Retry same push | 3 | Exponential + jitter (200ms base) |
| **Push non-fast-forward** (main moved) | Re-fetch, re-rebase, re-merge, re-push | 3 | Exponential + jitter |
| **Rebase transient errors** | `rebase_with_retry` | 3 | Exponential + jitter |
| **Merge conflict** | Return `MergeConflict` with file list | No retry | Task reopened for agent |
| **Commit rejected** (hooks) | Return `CommitRejected` with stdout/stderr | No retry | Surfaced to caller |
| **No changes to merge** | Return current HEAD SHA (idempotent) | N/A | N/A |

---

## 8. Pre-flight Health Check

Before any task can be dispatched, the Coordinator validates project health:

```mermaid
flowchart LR
    A[Coordinator<br/>validate_all_project_health] --> B{git remote get-url origin}
    B -- "No origin" --> C[Block dispatch<br/>project marked unhealthy]
    B -- "Has origin" --> D[Run setup commands]
    D --> E[Run verification commands]
    E --> F{All pass?}
    F -- Yes --> G[Project healthy<br/>dispatch allowed]
    F -- No --> C
```

A remote `origin` is required because the squash-merge flow pushes directly to it. Without it, tasks would loop infinitely (merge fails, task released, re-dispatched, fails again).

## Relations

- [[ADR-007 djinn Namespace Git Sync]]
- [[ADR-009 Simplified Execution]]
- [[ADR-015 Session Continuity and Resume]]