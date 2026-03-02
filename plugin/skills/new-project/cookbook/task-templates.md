# Task Templates Cookbook

Patterns for creating domain-structured task hierarchies with wave ordering. Use these as copy-paste templates when building task boards from planning artifacts.

## Hierarchy Quick Reference

| Level | Issue Type | Scope | Parent | Example |
|-------|-----------|-------|--------|---------|
| Epic | `epic` | Weeks+ of work, domain concept | None | "User Authentication System" |
| Feature | `feature` | 2-4 hours focused work | Epic | "JWT Token Management" |
| Task | `task` | One-commit outcome | Feature | "Create login API endpoint" |
| Bug | `bug` | Defect fix | Feature | "Fix token refresh race condition" |

**Sizing guidance:**
- **Epic**: A domain area containing multiple features. Named after WHAT it is, not WHEN it ships. Persists across milestones if the domain spans multiple phases.
- **Feature**: A coherent unit of work an agent can complete in one session. Has clear acceptance criteria and design context.
- **Task**: The smallest unit -- a single commit-sized change. Under a feature, provides one piece of the feature's functionality.
- **Bug**: A defect in existing functionality. Always under the feature it affects.

---

## Creation Patterns

### Epic Creation

Epics are domain-structured (per ADR-001). Name them after the domain concept, not the milestone.

```
task_create(
  title="User Authentication System",
  issue_type="epic",
  project="taskflow",
  description="Handles all user identity concerns: registration, login, session management, and token lifecycle. Spans project initialization (password-based auth) and core planning (token-gated API access).",
  acceptance_criteria=[
    "Users can register with email and password",
    "Users can log in and receive JWT tokens",
    "Refresh token rotation prevents session hijacking",
    "Protected endpoints reject unauthenticated requests"
  ],
  emoji="lock",
  color="blue"
)
```

**Key points:**
- `issue_type="epic"` -- no parent (top-level)
- `emoji` and `color` provide visual identity on the task board
- Description explains the domain scope, not the timeline
- Acceptance criteria are high-level outcomes for the entire epic

### Feature Creation

Features live under epics and represent focused, session-sized work.

```
task_create(
  title="JWT Token Management",
  issue_type="feature",
  project="taskflow",
  parent="k7m2",
  description="Implement JWT access and refresh token generation, validation, and rotation. Access tokens expire in 15 minutes; refresh tokens in 7 days with single-use rotation.",
  design="Use the jose library for JWT operations (ESM-native, Edge-compatible). Store refresh tokens in the database for revocation capability. Hash refresh tokens with SHA-256 before storage.",
  acceptance_criteria=[
    "Access token generated on successful login with 15-min expiry",
    "Refresh token generated alongside access token with 7-day expiry",
    "Refresh endpoint issues new token pair and invalidates old refresh token",
    "Expired or revoked tokens return 401 Unauthorized"
  ],
  memory_refs=["requirements/v1-requirements"]
)
```

**Key points:**
- `parent="k7m2"` links this feature to the epic (use the epic's actual ID from task_create response)
- `design` field captures implementation approach -- agents read this when working on the feature
- `acceptance_criteria` as an array of strings -- each is a testable condition
- `memory_refs` links to the requirements note this feature serves

### Task Creation

Tasks are one-commit outcomes under a feature.

```
task_create(
  title="Create login API endpoint",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="POST /api/auth/login endpoint that validates email/password credentials and returns a JWT token pair.",
  design="Validate request body (email, password required). Look up user by email. Compare password hash with bcrypt. If valid, generate access + refresh tokens via the JWT module. Set refresh token as httpOnly cookie. Return access token in response body.",
  acceptance_criteria=[
    {"criterion": "POST /api/auth/login accepts email and password", "met": false},
    {"criterion": "Returns 200 with access token on valid credentials", "met": false},
    {"criterion": "Returns 401 on invalid email or password", "met": false},
    {"criterion": "Sets httpOnly cookie with refresh token", "met": false}
  ],
  priority=1,
  labels=["wave:1"]
)
```

**Priority values (integer, 0=highest):**
- `0` = Critical (foundation tasks, wave 1)
- `1` = High (core logic, wave 2)
- `2` = Medium (integration, wave 3+)
- `3` = Low (nice-to-have, can be deferred)

**Key points:**
- `parent="p3x9"` links to the feature (use the feature's actual ID)
- `acceptance_criteria` uses the object format `{criterion, met}` for tasks -- `met` starts as `false` and is updated during execution
- `priority` field: integer (0=critical, 1=high, 2=medium, 3=low)
- `labels=["wave:1"]` tags the task for wave-based ordering (see Wave Ordering below)

### Bug Creation

Bugs report defects in existing functionality.

```
task_create(
  title="Refresh token rotation creates orphaned sessions",
  issue_type="bug",
  project="taskflow",
  parent="p3x9",
  description="When a refresh token is rotated, the old session record is not deleted from the database. After multiple refreshes, the sessions table accumulates stale records that consume storage and slow session lookups.",
  acceptance_criteria=[
    "Old session record is deleted when refresh token is rotated",
    "Sessions table contains only active sessions after rotation",
    "Session lookup performance does not degrade with token rotations"
  ],
  labels=["bug", "wave:2"],
  priority=1
)
```

**Key points:**
- `labels` includes `"bug"` tag plus wave assignment
- Description explains the observable problem, the cause, and the impact
- Parent is the feature where the bug was found

---

## Wave Ordering via Blocker Dependencies

Waves control execution order. Wave N+1 tasks are blocked by at least one Wave N task, ensuring dependencies are satisfied before downstream work begins.

**Pattern:** Only block on real technical or logical dependencies -- not arbitrary sequencing.

### Three-Wave Example

**Wave 1 -- Foundation (no blockers):**

```
# Wave 1 tasks have no blocked_by -- they can start immediately
wave1_task_a = task_create(
  title="Create user database schema",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="Define User table with id, email, password_hash, created_at columns.",
  acceptance_criteria=[
    {"criterion": "User table exists with required columns", "met": false},
    {"criterion": "Migration runs without errors", "met": false}
  ],
  labels=["wave:1"],
  priority=0
)
# Returns: { id: "a1b2", ... }

wave1_task_b = task_create(
  title="Set up JWT signing configuration",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="Configure JWT signing keys, token expiry durations, and algorithm selection.",
  acceptance_criteria=[
    {"criterion": "JWT signing key loaded from environment", "met": false},
    {"criterion": "Access token expiry set to 15 minutes", "met": false}
  ],
  labels=["wave:1"],
  priority=0
)
# Returns: { id: "c3d4", ... }
```

**Wave 2 -- Core logic (blocked by Wave 1):**

```
# Wave 2 tasks declare blocked_by pointing to Wave 1 task IDs
wave2_task = task_create(
  title="Create login API endpoint",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="POST /api/auth/login validates credentials and returns JWT tokens.",
  acceptance_criteria=[
    {"criterion": "Returns JWT pair on valid credentials", "met": false},
    {"criterion": "Returns 401 on invalid credentials", "met": false}
  ],
  blocked_by="a1b2",
  labels=["wave:2"],
  priority=1
)
# Returns: { id: "e5f6", ... }

# Add additional blockers after creation if needed
task_blockers_add(
  id="e5f6",
  blocking_id="c3d4",
  project="taskflow"
)
# Now e5f6 is blocked by both a1b2 (schema) AND c3d4 (JWT config)
```

**Wave 3 -- Integration (blocked by Wave 2):**

```
wave3_task = task_create(
  title="Add auth middleware to protected routes",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="Middleware that validates JWT access tokens on protected endpoints.",
  acceptance_criteria=[
    {"criterion": "Protected routes return 401 without valid token", "met": false},
    {"criterion": "Valid tokens pass through to route handler", "met": false}
  ],
  blocked_by="e5f6",
  labels=["wave:3"],
  priority=2
)
```

### Wave Ordering Rules

1. **Wave 1** tasks have no `blocked_by` -- they are the starting points
2. **Wave N+1** tasks set `blocked_by` to one Wave N task ID at creation. Use `task_blockers_add()` for additional blockers
3. **Schema note:** The `blocked_by` field accepts a single task ID string, not an array. To block on multiple tasks, use `blocked_by` for one and `task_blockers_add()` for each additional blocker
4. Use `task_blockers_add()` to add blockers after creation when a task depends on multiple Wave N tasks
5. Label convention: `labels=["wave:N"]` for easy filtering with `task_list(label="wave:1")`
6. **Only block on real dependencies** -- if two tasks CAN run in parallel, do not create an artificial blocker between them

---

## Memory-Task Bidirectional Linking

Tasks and memory notes should reference each other for traceability. Three patterns:

### Pattern 1: Link at Task Creation

When creating a task, use `memory_refs` to link to relevant memory notes:

```
task_create(
  title="Create login API endpoint",
  issue_type="task",
  project="taskflow",
  parent="p3x9",
  description="...",
  memory_refs=["requirements/v1-requirements", "decisions/adr-001-hierarchy-mapping"]
)
```

The `memory_refs` values are memory note permalinks (folder/slug format).

### Pattern 2: Add Links After Creation

Use `task_update` to add memory references to existing tasks:

```
task_update(
  id="e5f6",
  project="taskflow",
  memory_refs_add=["decisions/adr-002-state-derivation"]
)
```

### Pattern 3: Link Memory Notes Back to Tasks

Use `memory_edit` to add task references in the memory note's Relations section:

```
memory_edit(
  identifier="requirements/v1-requirements",
  operation="append",
  section="Relations",
  content="\n- Task e5f6: Create login API endpoint -- implements PLAN-01"
)
```

### Full Round-Trip Example

1. Create task with memory_refs:
   ```
   task_create(title="Create login API endpoint", ..., memory_refs=["requirements/v1-requirements"])
   # Returns: { id: "e5f6" }
   ```

2. Update memory note to link back:
   ```
   memory_edit(identifier="requirements/v1-requirements", operation="append", section="Relations", content="\n- Task e5f6: Create login API endpoint -- implements SETUP-01")
   ```

Now the task references the requirements note, and the requirements note references the task.

---

## Roadmap-to-Task-Board Mapping

Per ADR-001, milestones are narrative (roadmap memory note) and epics are domain concepts (task board). Here is the full mapping flow:

### The Flow

```
Roadmap (memory note, type=roadmap)
  Phase 2: Core Auth
    Requirements: PLAN-02, PLAN-03, PLAN-04
    Success Criteria: "Domain-structured epics on task board..."
        |
        v
Task Board (domain-structured)
  Epic: "User Authentication System"    (domain concept, not "Phase 2")
    Feature: "JWT Token Management"     (focused work unit)
      Task: "Create login endpoint"     (one-commit outcome, wave:1)
      Task: "Add refresh rotation"      (one-commit outcome, wave:2)
    Feature: "OAuth Integration"        (focused work unit)
      Task: "Configure OAuth provider"  (one-commit outcome, wave:1)
      Task: "Implement callback flow"   (one-commit outcome, wave:2)
```

### Key Principles (from ADR-001)

- **Milestones are narrative**: They live in the roadmap memory note and describe WHEN and WHY
- **Epics are domain concepts**: They live on the task board and describe WHAT
- **One milestone may touch multiple epics**: "Phase 2: Core Auth" creates tasks under both "Auth System" and "API Gateway" epics
- **One epic may span multiple milestones**: "Auth System" gets initial features in Phase 2 and OAuth features in Phase 3
- **Sequencing via blockers, not epic ordering**: Wave ordering handles execution sequence within and across features

### Mapping in Practice

1. Read roadmap note: `memory_read(identifier="roadmap")`
2. For each milestone, identify the domain areas (epics) that need work
3. Check if epics already exist: `task_list(issue_type="epic", project="taskflow")`
4. Create new epics only for new domain areas -- reuse existing epics
5. Create features under the appropriate epic
6. Create tasks under features with wave labels and blocker dependencies
7. Link tasks back to requirements via `memory_refs`

---

## Common Mistakes

1. **Naming epics after milestones.** "Milestone 1" or "Phase 2 Tasks" are timeline labels, not domain concepts. Name epics after what they ARE: "User Authentication System", "Research Pipeline", "Task Decomposition Engine". Per ADR-001, milestones belong in the roadmap note.

2. **Putting acceptance criteria in the description field.** The `description` field explains context and scope. The `acceptance_criteria` field (array of strings or `{criterion, met}` objects) holds the testable done conditions. Agents check `acceptance_criteria` to verify completion.

3. **Creating tasks without a parent.** Orphaned tasks (no parent feature or epic) break the hierarchy and make progress aggregation impossible. Every task needs a `parent` pointing to a feature; every feature needs a `parent` pointing to an epic.

4. **Setting blocked_by on features that could run in parallel.** Only create blocker dependencies for real technical or logical dependencies. If two wave-1 features can be worked on simultaneously, do NOT artificially sequence them. Over-constraining blockers reduces parallelism and slows execution.

5. **Forgetting to add memory_refs for traceability.** Every task should link to at least one memory note (usually requirements). Without `memory_refs`, there is no traceable path from requirement to implementation. Use `memory_refs` at creation or `task_update(memory_refs_add=...)` after.

6. **Using task_transition instead of letting the execution pipeline manage lifecycle.** Planning workflows create tasks in `open` status. Status transitions (`open` -> `in_progress` -> `review` -> `done`) are managed by the execution pipeline (Djinn coordinator), not by planning workflows. Planning never calls `task_transition`.

7. **Using array syntax for blocked_by.** The `blocked_by` field accepts a single task ID string, not an array. To set multiple blockers, use `blocked_by` at creation time for one, then `task_blockers_add()` for each additional blocker.

8. **Using string values for priority.** The `priority` field accepts an integer (0=critical, 1=high, 2=medium, 3=low), not a string like `"high"` or `"medium"`. Always use the integer form in `task_create` calls.
