# Task Templates Cookbook

Patterns for creating tasks with dependency-based ordering. Use these as copy-paste templates when creating tasks on the Djinn board.

## Hierarchy Quick Reference

| Level | Tool | Scope | Example |
|-------|------|-------|---------|
| Epic | `epic_create` | Domain concept, weeks+ | "User Authentication System" |
| Feature | `task_create(issue_type="feature")` | User-facing deliverable, 2-4h | "Login UI" |
| Task | `task_create(issue_type="task")` | Internal implementation, one commit | "Create JWT middleware" |
| Bug | `task_create(issue_type="bug")` | Defect fix | "Password field clears on error" |

**All task types (feature, task, bug) are flat siblings under an epic.** There is no parent-child relationship between them. Do NOT create a feature and then decompose it into child tasks.

Use `memory_refs` on `task_create` to set initial links. Use `task_update(memory_refs_add=..., memory_refs_remove=...)` to modify later.

---

## Creation Patterns

### Epic Creation

Epics are domain-structured (per ADR-001). Name them after the domain concept, not the milestone.

```
epic_create(
  project=PROJECT,
  title="User Authentication System",
  description="Handles all user identity concerns: registration, login, session management, and token lifecycle.",
  emoji="🔐",
  color="#8B5CF6"
)
```

### Task Creation

Tasks are the primary work unit. One task = one focused agent session.

```
task_create(
  project=PROJECT,
  title="Create login API endpoint",
  issue_type="task",
  epic_id="k7m2",
  description="POST /api/auth/login endpoint that validates email/password and returns a JWT token pair.",
  design="Validate request body (email, password required). Look up user by email. Compare password hash with bcrypt. Generate access + refresh tokens via JWT module. Set refresh token as httpOnly cookie.",
  acceptance_criteria=[
    {"criterion": "POST /api/auth/login accepts email and password", "met": false},
    {"criterion": "Returns 200 with access token on valid credentials", "met": false},
    {"criterion": "Returns 401 on invalid email or password", "met": false},
    {"criterion": "Sets httpOnly cookie with refresh token", "met": false}
  ],
  priority=1,
  memory_refs=["requirements/v1-requirements", "decisions/adr-005-jwt-session"]
)
# Returns: { id: "e5f6", ... }
```

Use `issue_type="feature"` for user-facing deliverables:

```
task_create(
  project=PROJECT,
  title="Login UI",
  issue_type="feature",
  epic_id="k7m2",
  description="Login page with email/password form. Entry point to the auth system.",
  design="LoginForm component using existing Form primitives. useAuth hook for API calls. On success: redirect to /dashboard. On failure: inline error, no page reload.",
  acceptance_criteria=[
    "Given valid credentials, redirect to dashboard",
    "Given invalid credentials, inline error without page reload",
    "Given expired session on protected route, redirect to login with return URL",
    "Form is accessible (WCAG 2.1 AA)"
  ],
  priority=1
)
```

**Features and tasks are peers.** Both live flat under an epic. Use `issue_type="feature"` when the work is user-facing, `issue_type="task"` when it's internal. Do not try to nest tasks under features.

### Bug Creation

```
task_create(
  project=PROJECT,
  title="Login fails with special chars in password",
  issue_type="bug",
  epic_id="k7m2",
  description="Passwords with '&' or '+' fail auth. Root cause: URL encoding issue in API call.",
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Password 'test+456' authenticates successfully",
    "Standard passwords still work (no regression)"
  ],
  priority=1
)
```

**Priority values (integer, 0=highest):**
- `0` = Critical (foundation, must go first)
- `1` = High (core logic)
- `2` = Medium (integration)
- `3` = Low (nice-to-have)

**Acceptance criteria must be code-testable.** Every criterion must be verifiable by an agent through unit tests, integration tests, or code inspection. Never use production metrics, manual verification, or environment-dependent checks.

Good: `"POST /api/login returns 401 with invalid credentials"`
Good: `"Unit test verifies JWT token contains user_id claim"`
Bad: `"System handles 1000 concurrent users in production"`
Bad: `"Stakeholder approves the design"`

---

## Dependency Ordering via Blockers

Execution order is controlled by blockers. **The Djinn coordinator dispatches any open task with no unresolved blockers IMMEDIATELY.** Use `blocked_by` on `task_create` to set blockers atomically -- the task is never visible without its blockers.

**Only block on real technical or logical dependencies.**

### Three-Stage Example

**Create foundation tasks first (no blockers), then downstream tasks with `blocked_by` referencing upstream IDs.**

**Stage 1 -- Foundation (no blockers, create these first):**

```
task_create(
  project=PROJECT,
  title="Create user database schema",
  issue_type="task",
  epic_id="k7m2",
  acceptance_criteria=[
    {"criterion": "User table exists with required columns", "met": false},
    {"criterion": "Migration runs without errors", "met": false}
  ],
  priority=0
)
# Returns: { id: "a1b2" }

task_create(
  project=PROJECT,
  title="Set up JWT signing configuration",
  issue_type="task",
  epic_id="k7m2",
  acceptance_criteria=[
    {"criterion": "JWT signing key loaded from environment", "met": false},
    {"criterion": "Access token expiry set to 15 minutes", "met": false}
  ],
  priority=0
)
# Returns: { id: "c3d4" }
```

**Stage 2 -- Core logic (blocked by Stage 1):**

```
task_create(
  project=PROJECT,
  title="Create login API endpoint",
  issue_type="task",
  epic_id="k7m2",
  acceptance_criteria=[
    {"criterion": "Returns JWT pair on valid credentials", "met": false},
    {"criterion": "Returns 401 on invalid credentials", "met": false}
  ],
  priority=1,
  blocked_by=["a1b2", "c3d4"]
)
# Returns: { id: "e5f6" } -- created already blocked, never dispatched prematurely
```

**Stage 3 -- Integration (blocked by Stage 2):**

```
task_create(
  project=PROJECT,
  title="Add auth middleware to protected routes",
  issue_type="task",
  epic_id="k7m2",
  acceptance_criteria=[
    {"criterion": "Protected routes return 401 without valid token", "met": false},
    {"criterion": "Valid tokens pass through to route handler", "met": false}
  ],
  priority=2,
  blocked_by=["e5f6"]
)
# Returns: { id: "g7h8" }
```

### Ordering Rules

1. **Create tasks in dependency order** -- foundation first, then downstream
2. **Use `blocked_by` on `task_create`** to set blockers atomically at creation time
3. Only block on real dependencies -- if two tasks CAN run in parallel, let them
4. Use `task_update(blocked_by_add=..., blocked_by_remove=...)` to adjust blockers after creation

---

## Memory-Task Linking

Set `memory_refs` at creation, or modify later with `task_update`.

### Set memory refs at creation

```
task_create(
  project=PROJECT,
  title="Create login API endpoint",
  issue_type="task",
  ...,
  memory_refs=["requirements/v1-requirements", "decisions/adr-002-state-derivation"]
)
```

### Add/remove refs later

```
task_update(
  project=PROJECT,
  id="e5f6",
  memory_refs_add=["decisions/adr-005-jwt-session"],
  memory_refs_remove=["decisions/adr-002-state-derivation"]
)
```

### Link memory notes back to tasks

```
memory_edit(
  identifier="requirements/v1-requirements",
  operation="append",
  section="Relations",
  content="\n- Task e5f6: Create login API endpoint -- implements SETUP-01"
)
```

### Full round-trip

1. Create task with refs: `task_create(project=PROJECT, ..., memory_refs=["requirements/v1-requirements"])` -> `{ id: "e5f6" }`
2. Add backlink to memory: `memory_edit(identifier="requirements/v1-requirements", operation="append", section="Relations", content="\n- Task e5f6: Create login API endpoint")`

---

## Common Mistakes

1. **Decomposing features into child tasks.** Features, tasks, and bugs are flat siblings under an epic. There is no nesting. If something is too large, split it into multiple independent tasks at the same level.

3. **Naming epics after milestones.** "Phase 2 Tasks" is a timeline label. Name epics after domain concepts: "User Authentication System".

4. **Putting acceptance criteria in description.** Use the `acceptance_criteria` array field. Description is for context.

5. **Adding unnecessary blockers.** Only block on real dependencies. Over-constraining reduces parallelism.

6. **Using string values for priority.** Priority is an integer (0-3), not a string.

7. **Omitting `project` on task/epic tools.** Always pass `project=PROJECT`.

8. **Not using `blocked_by` on `task_create`.** Always pass `blocked_by` at creation time to set blockers atomically. The coordinator dispatches unblocked tasks immediately.

9. **Creating downstream tasks before upstream tasks.** The `blocked_by` IDs must exist at creation time. Always create foundation tasks first.

10. **Using non-code-testable acceptance criteria.** "Handles 1000 concurrent users" or "stakeholder approves" cannot be verified by an agent. Use criteria testable via unit/integration tests or code inspection.

11. **Not checking for premature dispatch.** If you created a task without blockers by mistake, check `session_for_task` to see if a session already picked it up.
