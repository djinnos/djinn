# Work Decomposition Cookbook

How to structure work into epics and tasks in djinn -- workflow agnostic.

## The Hierarchy

Epics and tasks use **separate MCP tools** (per ADR-003):

```
epic_create(project=PROJECT, ...)  -> Epic: "User Authentication System"   (weeks, strategic container)
task_create(project=PROJECT, ...)  -> Feature: "Login UI"                   (2-4h, user-facing deliverable)
task_create(project=PROJECT, ...)  -> Task: "Create JWT middleware"          (1 outcome, internal implementation)
task_create(project=PROJECT, ...)  -> Bug: "Password field clears on error" (defect fix)
```

Features, tasks, and bugs are **all flat siblings under an epic**. There is no nesting of tasks under features. The `issue_type` field distinguishes them but they share the same parent level.

**Rule of thumb:**
- If an agent can't implement it in one focused session -> it's too big, split into independent peers
- If it doesn't produce a testable outcome -> it's too vague, add acceptance criteria
- If it depends on unreleased work -> add a blocker

## Epics

Epics are **strategic containers** managed by their own tool namespace. They don't get implemented directly -- their child tasks do.

```
epic_create(
  project=PROJECT,
  title="User Authentication System",
  emoji="🔐",
  color="#8B5CF6",
  description="Complete authentication enabling secure access to all user-specific features."
)
```

**Good epic characteristics:**
- Describes a domain concept, not a timeline phase
- Contains 2-8 tasks/features (if more, consider splitting)
- Use `epic_tasks(project=PROJECT, epic_id=...)` to list children

## Tasks

Tasks are the primary unit of work. Each should be completable in one focused agent session.

```
# Step 1: Create the task
task_create(
  project=PROJECT,
  title="Create login API endpoint",
  issue_type="task",
  epic_id="k7m2",
  description="POST /api/auth/login endpoint that validates email/password and returns JWT tokens.",
  design="Validate request body. Look up user by email. Compare password hash with bcrypt. Generate tokens via JWT module. Set refresh token as httpOnly cookie.",
  acceptance_criteria=[
    {"criterion": "POST /api/auth/login accepts email and password", "met": false},
    {"criterion": "Returns 200 with access token on valid credentials", "met": false},
    {"criterion": "Returns 401 on invalid email or password", "met": false}
  ],
  priority=1,
  memory_refs=["requirements/v1-requirements"]
)
# Returns: { id: "e5f6" }

# Add more refs later if needed
task_update(
  project=PROJECT,
  id="e5f6",
  memory_refs_add=["decisions/adr-005-jwt-session"]
)
```

Use `issue_type="feature"` for user-facing deliverables:

```
task_create(
  project=PROJECT,
  title="Login UI",
  issue_type="feature",
  epic_id="k7m2",
  description="Login page with email/password form. Entry point to the auth system.",
  design="LoginForm component using Form primitives. useAuth hook for API calls. Redirect to /dashboard on success. Inline error on failure.",
  acceptance_criteria=[
    "Given valid credentials, redirect to dashboard",
    "Given invalid credentials, inline error without page reload",
    "Form is accessible (WCAG 2.1 AA)"
  ],
  priority=1
)
```

**Features and tasks are peers.** Both sit flat under an epic. The distinction is semantic (user-facing vs internal), not hierarchical.

## Bugs

```
task_create(
  project=PROJECT,
  title="Login fails with special chars in password",
  issue_type="bug",
  epic_id="k7m2",
  description="Passwords with '&' or '+' fail auth. Root cause: URL encoding issue.",
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Standard passwords still work (no regression)"
  ],
  priority=1
)
```

## Sizing

| Size | Effort | Action |
|------|--------|--------|
| XS | < 1h | Fine as-is |
| S | 1-2h | Fine as-is |
| M | 2-4h | Target size |
| L | 4-8h | Split into 2-3 independent tasks |
| XL | > 8h | Split into multiple tasks, possibly a new epic |

When splitting, create independent peer tasks -- NOT parent-child:
```
# Too large: "User authentication"
# Split into independent peer tasks:
  "Login UI"             (feature, M)
  "Registration flow"    (feature, M)
  "Email verification"   (feature, M)
  "JWT middleware"        (task, S)
  "Session management"   (task, S)
```

## Dependency Ordering

**Blockers are THE execution sequence mechanism.** The Djinn coordinator dispatches any open task with no unresolved blockers. If you don't set blockers, tasks run in parallel.

```
# Registration must exist before email verification can be built
task_blockers_add(
  project=PROJECT,
  id="email-verification-id",
  blocking_id="registration-id",
)
```

**When to add blockers:**
- Technical dependency: A must ship before B can be built
- Data dependency: B needs data that A creates
- Build dependency: B imports code that A produces

**When NOT to add blockers:**
- "Nice to have" sequencing
- Tasks in completely different areas
- Personal preference about order

## Labels for Grouping

Labels enable cross-cutting queries:

```
labels=["area:auth", "sprint:3", "layer:api"]
```

Query examples:
```
task_list(project=PROJECT, label="sprint:3", text="auth")
task_list(project=PROJECT, label="layer:api", issue_type="task")
task_count(project=PROJECT, group_by="epic")
```

## Memory-Task Linking

Set `memory_refs` at creation, or add/remove later with `task_update`:

```
# At creation
task_create(project=PROJECT, ..., memory_refs=["requirements/v1-requirements"])

# Or add/remove later
task_update(project=PROJECT, id="e5f6", memory_refs_add=["decisions/adr-005"])

# Add backlink to memory note
memory_edit(
  identifier="requirements/v1-requirements",
  operation="append",
  section="Relations",
  content="\n- Task e5f6: Create login API endpoint -- implements AUTH-01"
)
```

## Acceptance Criteria Patterns

```python
# Given/When/Then (behavioral)
"Given valid credentials, when user submits, then they are redirected to /dashboard"

# Observable outcome
"Form submits without page reload"

# Testable assertion
"Unit test: middleware rejects expired tokens with 401"
```

**Bad AC (avoid):**
```
"Works correctly"           # How do we know?
"Is tested"                 # What tests?
"Follows best practices"    # Which ones?
```

## Common Mistakes

1. **Decomposing features into child tasks.** Features, tasks, and bugs are flat siblings. Split large items into independent peers instead.

2. **Naming epics after milestones.** Name epics after domain concepts: "User Authentication System", not "Phase 2".

4. **Putting acceptance criteria in description.** Use the `acceptance_criteria` array field.

5. **Adding unnecessary blockers.** Only block on real dependencies. Let the coordinator parallelize the rest.

6. **Using string values for priority.** Priority is an integer (0-3).

7. **Omitting `project` on task/epic tools.** Always pass `project=PROJECT`.

8. **Trying to pass `blocked_by` to `task_create`.** Use `task_blockers_add()` after creation.
