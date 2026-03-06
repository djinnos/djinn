# Task Management Cookbook

Complete guide to creating, updating, querying, and transitioning djinn tasks.

## Task Anatomy

Every task has these key fields:

| Field | Purpose | Notes |
|-------|---------|-------|
| `title` | One-line summary | Imperative form: "Add login endpoint" |
| `issue_type` | feature / task / bug | Work item type (all flat under epic) |
| `description` | Context and background | NOT for acceptance criteria |
| `acceptance_criteria` | What done looks like | Array of strings or {criterion, met} objects |
| `design` | How to implement | ADR refs, technical approach, architecture notes |
| `priority` | 0=highest, higher=lower | 0 for blocking/critical work |
| `epic_id` | Parent epic ID | Optional — use when grouping work under an epic |
| `labels` | Arbitrary tags | Use for sprint:X, area:auth grouping |
| `status` | Current state | See status flow in SKILL.md |
| `owner` | Assigned agent/user | email format |

## Creating Tasks

### Epic (top-level container — separate tool)
```
epic_create(
  project=PROJECT,
  title="User Authentication System",
  emoji="🔐",
  color="#8B5CF6",       # Must be hex format
  description="Implement complete auth enabling secure access. Blocks all user-specific features."
)
```

Epics are managed via their own tools: `epic_create`, `epic_list`, `epic_show`, `epic_tasks`, `epic_update`, `epic_close`, `epic_reopen`, `epic_delete`, `epic_count`.

### Feature (deliverable, 2-4h scope)
```
task_create(
  project=PROJECT,
  title="Login UI",
  issue_type="feature",
  epic_id="epic-short-id",
  description="User can log in with email/password. Entry point for auth system.",
  design="LoginForm component. useAuth hook for API. Redirect to dashboard on success.",
  acceptance_criteria=[
    "Given valid credentials, user is redirected to dashboard",
    "Given invalid credentials, error displays without page reload",
    "Given expired session on protected route, redirect to login with return URL"
  ],
  priority=1
)
```

### Task (implementation step)
```
task_create(
  project=PROJECT,
  title="Create auth middleware",
  issue_type="task",
  epic_id="epic-short-id",
  description="JWT validation middleware for protected routes.",
  design="Use existing session package. See ADR-005 for token format.",
  acceptance_criteria=[
    "Middleware validates JWT signature",
    "Expired tokens return 401 with clear error",
    "Valid tokens set user context on request"
  ],
  priority=0,
  labels=["sprint:3", "area:auth"]
)
```

### Bug
```
task_create(
  project=PROJECT,
  title="Login fails with special characters in password",
  issue_type="bug",
  epic_id="epic-short-id",
  description="Passwords containing '&' or '+' fail authentication. Discovered during edge case testing.",
  design="Root cause: URL encoding issue in API call. Fix: encodeURIComponent on password.",
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Password 'test+456' authenticates successfully"
  ],
  priority=1,
  blocked_by=["other-task-id"]  # optional: set blockers atomically at creation
)

```

## Claiming and Starting Work

### Claim next available task
```
task_claim(
  project=PROJECT,
  issue_type="task",      # Filter to tasks only
  label="sprint:3",       # Optional: filter by sprint
  priority_max=2          # Only grab up to priority 2
)
```

### Start a specific task
```
task_transition(id="task-id", action="start")
```

### List ready tasks (no blockers, open status)
```
task_ready(
  project=PROJECT,
  limit=10
)
```

## Progress Notes (Critical for Resume)

Always add comments at key moments. This enables any agent or human to resume with full context.

```
# Starting work
task_comment_add(
  project=PROJECT,
  id="task-id",
  body="[STARTING] Approach: implement JWT middleware using existing session package. ADR-005 applies."
)

# Mid-task progress
task_comment_add(
  project=PROJECT,
  id="task-id",
  body="[PROGRESS] Middleware validates tokens. Next: add user context injection and error responses."
)

# Blocked
task_comment_add(
  project=PROJECT,
  id="task-id",
  body="[BLOCKED] Waiting for session package update (#123). Token format changed in latest version."
)

# Pausing session
task_comment_add(
  project=PROJECT,
  id="task-id",
  body="[PAUSED] Completed: token validation, error handling. Next: user context injection. Branch: feature/auth-middleware"
)

# Done
task_comment_add(
  project=PROJECT,
  id="task-id",
  body="[DONE] Implemented JWT middleware with validation, error handling, user context. All tests passing."
)
```

## Status Transitions

### Full transition reference

| From | Action | To | When |
|------|--------|----|------|
| draft | accept | open | Approve for work |
| open | start | in_progress | Begin working |
| in_progress | submit_task_review | needs_task_review | Work done, request review |
| in_progress | close | closed | Skip review (quick close) |
| needs_task_review | task_review_start | in_task_review | Start reviewing |
| needs_task_review | task_review_approve | needs_epic_review | Task approved |
| needs_task_review | task_review_reject | in_progress | Send back for fixes |
| needs_epic_review | epic_review_approve | closed | Final approval |
| needs_epic_review | epic_review_reject | in_progress | Major rework needed |
| any | block | blocked | Hit blocker |
| blocked | unblock | previous | Blocker resolved |
| closed | reopen | open | Needs more work |
| any | force_close | closed | Force close (with reason) |

### Quick close (no review)
```
task_transition(id="task-id", action="close")
```

### Request review
```
task_transition(id="task-id", action="submit_task_review")
```

### Reopen
```
task_transition(
  project=PROJECT,
  id="task-id",
  action="reopen",
  reason="Found regression in edge case"
)
```

## Blocker Management

### Set blockers at creation (preferred)
```
# Task B is blocked by Task A -- set atomically at creation
task_create(project=PROJECT, title="Task B", ..., blocked_by=["task-a-id"])
```

### Add/remove blockers after creation
```
task_update(project=PROJECT, id="task-b-id", blocked_by_add=["task-a-id"])
task_update(project=PROJECT, id="task-b-id", blocked_by_remove=["task-a-id"])
```

### List what blocks a task
```
task_blockers_list(project=PROJECT, id="task-id")
```

### List what a task blocks
```
task_blocked_list(project=PROJECT, id="task-id")
```

## Querying Tasks

### Filter by status
```
task_list(project=PROJECT, status="in_progress")
task_list(project=PROJECT, status="needs_task_review")
```

### Filter by type
```
task_list(project=PROJECT, issue_type="bug")
```

### Get all children of an epic
```
epic_tasks(project=PROJECT, epic_id="epic-id")
```

### Text search
```
task_list(project=PROJECT, text="authentication")
```

### Count by status
```
task_count(project=PROJECT, group_by="status")
```

### Get epic children
```
epic_tasks(project=PROJECT, epic_id="epic-id")
```

### Paginate large result sets
```
# Page 1
task_list(project=PROJECT, limit=25, offset=0)

# Page 2
task_list(project=PROJECT, limit=25, offset=25)
```

## Updating Tasks

### Update fields
```
task_update(
  project=PROJECT,
  id="task-id",
  title="Better title",
  priority=0,
  labels_add=["sprint:4"],
  labels_remove=["sprint:3"],
  acceptance_criteria=[
    "New criterion 1",
    "New criterion 2"
  ]
)
```

### Add/remove labels
```
# Add labels
task_update(project=PROJECT, id="task-id", labels_add=["area:auth", "sprint:3"])

# Remove labels
task_update(project=PROJECT, id="task-id", labels_remove=["old-label"])
```

### Link to memory note
```
task_update(
  project=PROJECT,
  id="task-id",
  memory_refs_add=["decisions/auth-strategy.md"]
)
```

## Sorting

```
# Default: priority ASC (best for "what to work on next")
task_list(project=PROJECT, sort="priority")

# Recently closed (best for "what was just done")
task_list(project=PROJECT, status="closed", sort="closed")

# Recently updated
task_list(project=PROJECT, sort="updated_desc")
```

## Project-Scoped Queries

Task and epic board tools are project-scoped; always pass `project`:
```
task_list(project=PROJECT, status="in_progress")
task_claim(project=PROJECT)
task_ready(project=PROJECT)
```
