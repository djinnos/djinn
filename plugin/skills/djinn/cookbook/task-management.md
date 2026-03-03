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
| `epic_id` | Parent epic ID | Required — every task/feature/bug belongs to an epic |
| `labels` | Arbitrary tags | Use for sprint:X, area:auth grouping |
| `status` | Current state | See status flow in SKILL.md |
| `owner` | Assigned agent/user | email format |

## Creating Tasks

### Epic (top-level container — separate tool)
```
epic_create(
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
  title="Login fails with special characters in password",
  issue_type="bug",
  epic_id="epic-short-id",
  description="Passwords containing '&' or '+' fail authentication. Discovered during edge case testing.",
  design="Root cause: URL encoding issue in API call. Fix: encodeURIComponent on password.",
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Password 'test+456' authenticates successfully"
  ],
  priority=1
)
# If it depends on something, add a blocker after creation:
# task_blockers_add(id="this-bug-id", blocking_id="other-task-id")

```

## Claiming and Starting Work

### Claim next available task
```
task_claim(
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
  limit=10
)
```

## Progress Notes (Critical for Resume)

Always add comments at key moments. This enables any agent or human to resume with full context.

```
# Starting work
task_comment_add(
  id="task-id",
  body="[STARTING] Approach: implement JWT middleware using existing session package. ADR-005 applies."
)

# Mid-task progress
task_comment_add(
  id="task-id",
  body="[PROGRESS] Middleware validates tokens. Next: add user context injection and error responses."
)

# Blocked
task_comment_add(
  id="task-id",
  body="[BLOCKED] Waiting for session package update (#123). Token format changed in latest version."
)

# Pausing session
task_comment_add(
  id="task-id",
  body="[PAUSED] Completed: token validation, error handling. Next: user context injection. Branch: feature/auth-middleware"
)

# Done
task_comment_add(
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
| needs_task_review | task_review_approve | needs_phase_review | Task approved |
| needs_task_review | task_review_reject | in_progress | Send back for fixes |
| needs_phase_review | phase_review_approve | closed | Final approval |
| needs_phase_review | phase_review_reject | in_progress | Major rework needed |
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
  id="task-id",
  action="reopen",
  reason="Found regression in edge case"
)
```

## Blocker Management

### Add blocker relationship
```
# Task B is blocked by Task A
task_blockers_add(id="task-b-id", blocking_id="task-a-id")
```

### List what blocks a task
```
task_blockers_list(id="task-id")
```

### List what a task blocks
```
task_blocked_list(id="task-id")
```

### Remove blocker
```
task_blockers_remove(id="task-id", blocking_id="task-a-id")
```

## Querying Tasks

### Filter by status
```
task_list(status="in_progress")
task_list(status="needs_task_review")
```

### Filter by type
```
task_list(issue_type="bug")
```

### Get all children of an epic
```
epic_tasks(epic_id="epic-id")
```

### Text search
```
task_list(text="authentication")
```

### Count by status
```
task_count(group_by="status")
```

### Get epic children
```
epic_tasks(epic_id="epic-id")
```

### Paginate large result sets
```
# Page 1
task_list(limit=25, offset=0)

# Page 2
task_list(limit=25, offset=25)
```

## Updating Tasks

### Update fields
```
task_update(
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
task_update(id="task-id", labels_add=["area:auth", "sprint:3"])

# Remove labels
task_update(id="task-id", labels_remove=["old-label"])
```

### Link to memory note
```
task_update(
  id="task-id",
  memory_refs_add=["decisions/auth-strategy.md"]
)
```

## Sorting

```
# Default: priority ASC (best for "what to work on next")
task_list(sort="priority")

# Recently closed (best for "what was just done")
task_list(status="closed", sort="closed")

# Recently updated
task_list(sort="updated_desc")
```

## Multi-Project Queries

Omit `project` to search across all projects:
```
task_list(status="in_progress")    # All in-progress tasks everywhere
task_claim()                        # Claim from any project
task_ready()                        # Ready tasks from all projects
```
