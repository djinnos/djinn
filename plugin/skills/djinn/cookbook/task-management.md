# Task Management Cookbook

Complete guide to creating, updating, querying, and transitioning djinn tasks.

## Task Anatomy

Every task has these key fields:

| Field | Purpose | Notes |
|-------|---------|-------|
| `title` | One-line summary | Imperative form: "Add login endpoint" |
| `issue_type` | epic / feature / task / bug | Determines hierarchy position |
| `description` | Context and background | NOT for acceptance criteria |
| `acceptance_criteria` | What done looks like | Array of strings or {criterion, met} objects |
| `design` | How to implement | ADR refs, technical approach, architecture notes |
| `priority` | 0=highest, higher=lower | 0 for blocking/critical work |
| `parent` | Parent epic/feature ID | Required for features, tasks, bugs |
| `labels` | Arbitrary tags | Use for sprint:X, epic:Y, area:auth grouping |
| `status` | Current state | See status flow in SKILL.md |
| `owner` | Assigned agent/user | email format |
| `blocked_by` | Blocking task ID | Set at creation, or use task_blockers_add |

## Creating Tasks

### Epic (top-level initiative)
```
task_create(
  title="User Authentication System",
  issue_type="epic",
  project="/path/to/project",
  emoji="🔐",           # Required for epics
  color="#8b5cf6",      # Optional, auto-assigned if omitted
  description="Implement complete auth enabling secure access. Blocks all user-specific features.",
  acceptance_criteria=[
    "Users can register with email/password",
    "Users can log in and receive persistent session",
    "Protected routes reject unauthenticated requests"
  ],
  priority=1
)
```

### Feature/Story (deliverable, 2-4h scope)
```
task_create(
  title="Login UI",
  issue_type="feature",
  parent="epic-id",
  project="/path/to/project",
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
  parent="feature-id",
  project="/path/to/project",
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
  parent="feature-id",
  project="/path/to/project",
  description="Passwords containing '&' or '+' fail authentication. Discovered during edge case testing.",
  design="Root cause: URL encoding issue in API call. Fix: encodeURIComponent on password.",
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Password 'test+456' authenticates successfully"
  ],
  priority=1,
  blocked_by="other-task-id"   # If it depends on something first
)
```

## Claiming and Starting Work

### Claim next available task
```
task_claim(
  project="/path/to/project",
  issue_type="task",      # Filter to tasks only
  label="sprint:3",       # Optional: filter by sprint
  priority_max=2          # Only grab up to priority 2
)
```

### Start a specific task
```
task_transition(id="task-id", action="start", project="...")
```

### List ready tasks (no blockers, open status)
```
task_ready(
  project="/path/to/project",
  issue_type="!epic",     # Exclude epics
  limit=10
)
```

## Progress Notes (Critical for Resume)

Always add comments at key moments. This enables any agent or human to resume with full context.

```
# Starting work
task_comment_add(
  id="task-id",
  body="[STARTING] Approach: implement JWT middleware using existing session package. ADR-005 applies.",
  project="..."
)

# Mid-task progress
task_comment_add(
  id="task-id",
  body="[PROGRESS] Middleware validates tokens. Next: add user context injection and error responses.",
  project="..."
)

# Blocked
task_comment_add(
  id="task-id",
  body="[BLOCKED] Waiting for session package update (#123). Token format changed in latest version.",
  project="..."
)

# Pausing session
task_comment_add(
  id="task-id",
  body="[PAUSED] Completed: token validation, error handling. Next: user context injection. Branch: feature/auth-middleware",
  project="..."
)

# Done
task_comment_add(
  id="task-id",
  body="[DONE] Implemented JWT middleware with validation, error handling, user context. All tests passing.",
  project="..."
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
task_transition(id="task-id", action="close", project="...")
```

### Request review
```
task_transition(id="task-id", action="submit_task_review", project="...")
```

### Reopen
```
task_transition(
  id="task-id",
  action="reopen",
  reason="Found regression in edge case",
  project="..."
)
```

## Blocker Management

### Add blocker relationship
```
# Task B is blocked by Task A
task_blockers_add(id="task-b-id", blocking_id="task-a-id", project="...")
```

### List what blocks a task
```
task_blockers_list(id="task-id", project="...")
```

### List what a task blocks
```
task_blocked_list(id="task-id", project="...")
```

### Remove blocker
```
task_blockers_remove(id="task-id", blocking_id="task-a-id", project="...")
```

## Querying Tasks

### Filter by status
```
task_list(project="...", status="in_progress")
task_list(project="...", status="needs_task_review")
```

### Filter by type
```
task_list(project="...", issue_type="bug")
task_list(project="...", issue_type="!epic")  # All non-epics
```

### Filter by parent (get all children of an epic)
```
task_list(project="...", parent="epic-id")
```

### Text search
```
task_list(project="...", text="authentication")
```

### Count by status
```
task_count(project="...", group_by="status")
```

### Get epic children
```
task_children_list(epic_id="epic-id", project="...")
```

### Paginate large result sets
```
# Page 1
task_list(project="...", limit=25, offset=0)

# Page 2
task_list(project="...", limit=25, offset=25)
```

## Updating Tasks

### Update fields
```
task_update(
  id="task-id",
  project="...",
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
task_update(id="task-id", project="...", labels_add=["area:auth", "sprint:3"])

# Remove labels
task_update(id="task-id", project="...", labels_remove=["old-label"])
```

### Link to memory note
```
task_update(
  id="task-id",
  project="...",
  memory_refs_add=["decisions/auth-strategy.md"]
)
```

## Sorting

```
# Default: priority ASC (best for "what to work on next")
task_list(project="...", sort="priority")

# Recently closed (best for "what was just done")
task_list(project="...", status="closed", sort="closed")

# Recently updated
task_list(project="...", sort="updated_desc")
```

## Multi-Project Queries

Omit `project` to search across all projects:
```
task_list(status="in_progress")    # All in-progress tasks everywhere
task_claim()                        # Claim from any project
task_ready()                        # Ready tasks from all projects
```
