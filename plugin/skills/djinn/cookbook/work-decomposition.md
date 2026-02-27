# Work Decomposition Cookbook

How to structure work into epics, features, and tasks in djinn — workflow agnostic.

## The Core Hierarchy

```
Epic           → "User Authentication System"        (weeks, strategic initiative)
  Feature      → "Login UI"                          (2-4h, one deliverable)
    Task       → "Create JWT middleware"              (1 outcome, implementable)
    Task       → "Add login form component"
    Bug        → "Password field clears on error"
  Feature      → "Registration Flow"
    Task       → "...
```

**Rule of thumb:**
- If a dev (or agent) can't implement it in one focused session → it's too big, split further
- If it doesn't produce a testable outcome → it's too vague, add acceptance criteria
- If it depends on unreleased work → add a blocker

## Epics

Epics are **strategic containers**. They don't get implemented directly — their child features do.

```
task_create(
  title="User Authentication System",
  issue_type="epic",
  emoji="🔐",
  description="""
  Complete authentication enabling secure access to all user-specific features.
  Blocks the entire user-facing product — must ship before any personalization.
  """,
  acceptance_criteria=[
    "Users can register, verify email, and log in",
    "Sessions persist across browser restarts",
    "Protected routes reject unauthenticated access",
    "Password reset works end-to-end"
  ],
  priority=0,
  project="..."
)
```

**Good epic characteristics:**
- Describes a user-facing capability, not a technical component
- Has acceptance criteria that a non-technical stakeholder can verify
- Contains 2-8 features (if more, consider splitting the epic)

## Features / Stories

Features are the primary unit of delivery. Each feature should be completable in one focused agent session (2-4 hours).

```
task_create(
  title="Login UI",
  issue_type="feature",
  parent="epic-id",
  description="""
  As a user, I want to log in with email/password so I can access my account.
  This is the entry point to the auth system — must handle all edge cases gracefully.
  """,
  design="""
  LoginForm component using existing Form primitives.
  useAuth hook for API calls. On success: redirect to /dashboard.
  On failure: inline error, no page reload. See ADR-005 for session handling.
  """,
  acceptance_criteria=[
    "Given valid credentials → redirect to dashboard",
    "Given invalid credentials → inline error, no page reload",
    "Given expired session on protected route → redirect to login with return URL",
    "Form is accessible (WCAG 2.1 AA)"
  ],
  priority=1,
  project="..."
)
```

**Good feature characteristics:**
- User-story framing ("As a user, I want...")
- Design field contains the APPROACH (not acceptance criteria — those go in AC)
- AC written as "Given/When/Then" or observable outcomes
- Priority set relative to other features in the epic

## Tasks

Tasks are implementation steps. One task = one atomic commit.

```
task_create(
  title="Implement JWT validation middleware",
  issue_type="task",
  parent="feature-id",
  description="""
  JWT validation middleware for protected Express routes.
  Validates RS256 signed tokens, rejects expired tokens with clear error.
  """,
  design="""
  Use jsonwebtoken library (already installed). Public key from env JWT_PUBLIC_KEY.
  Follow middleware pattern from ADR-005. Set req.user on success.
  Reference: server/middleware/auth-existing.js for pattern.
  """,
  acceptance_criteria=[
    "Valid JWT → sets req.user with decoded payload",
    "Expired JWT → 401 with {error: 'token_expired'}",
    "Invalid signature → 401 with {error: 'invalid_token'}",
    "Missing token → 401 with {error: 'auth_required'}",
    "Unit tests cover all four cases"
  ],
  priority=0,
  labels=["area:auth", "sprint:3"],
  project="..."
)
```

**Good task characteristics:**
- `design` tells EXACTLY how to implement (references ADRs, patterns, existing code)
- AC is specific enough that an agent can write tests for it
- Single responsibility — one thing, one commit
- Labels for grouping (sprint, area, feature-flag)

## Bugs

Bugs are defects found during or after implementation.

```
task_create(
  title="Login fails with special chars in password",
  issue_type="bug",
  parent="feature-id",
  description="""
  Passwords with '&' or '+' fail auth. Found during edge case testing of login flow.
  Reproducible: test@example.com / pass&word123 → 401 despite valid credentials.
  """,
  design="""
  Root cause: URL encoding issue in API call. The password isn't encoded before
  being sent in the request body. Fix: use JSON body (not form-encoded) or
  encodeURIComponent before sending.
  """,
  acceptance_criteria=[
    "Password 'test&123' authenticates successfully",
    "Password 'test+456' authenticates successfully",
    "Standard passwords still work (no regression)"
  ],
  priority=1,
  project="..."
)
```

## Sizing Features

Feature sizing guide — each feature should produce a working, testable deliverable:

| Size | Effort | Examples |
|------|--------|---------|
| XS | < 1h | Add a field to a form, update a constant, fix a typo |
| S | 1-2h | Single API endpoint, simple UI component, a hook |
| M | 2-4h | ✓ Target size — login form, email verification flow, data table |
| L | 4-8h | Too large — split into 2-3 features |
| XL | > 8h | Way too large — split into multiple features, possibly a new epic |

When a feature is L or XL, split it:
```
# Too large: "User authentication"
→ Split into:
  "Login UI"           (S)
  "Registration flow"  (M)
  "Email verification" (M)
  "Password reset"     (M)
  "Session management" (S)
```

## Dependency Mapping

Use blockers to express sequencing requirements:

```
# Registration must exist before email verification can be built
task_blockers_add(
  id="email-verification-feature-id",
  blocking_id="registration-feature-id",
  project="..."
)
```

**When to add blockers:**
- Technical dependency: A must ship before B can be built
- Logical dependency: B assumes A's UI/API exists
- Data dependency: B needs data that A creates

**When NOT to add blockers:**
- "Nice to have" sequencing — let execution engine parallelize
- Features in completely different areas (auth vs. billing)
- Personal preference about order

## Labels for Grouping

Labels enable cross-cutting queries without modifying the hierarchy:

```
# Sprint tracking
labels=["sprint:3"]

# Domain/area grouping
labels=["area:auth", "area:payments"]

# Feature flags
labels=["flag:new-checkout"]

# Layer
labels=["layer:api", "layer:ui", "layer:db"]

# Special
labels=["hotfix", "tech-debt", "a11y"]
```

Query examples:
```
# All auth work in sprint 3
task_list(project="...", label="sprint:3", text="auth")

# All API tasks
task_list(project="...", label="layer:api", issue_type="task")

# Count by area
task_count(project="...", group_by="parent")
```

## Acceptance Criteria Patterns

Well-written AC enables agent verification and review:

```python
# Given/When/Then (behavioral)
"Given valid credentials, when user submits, then they are redirected to /dashboard"
"Given invalid credentials, when user submits, then error message appears inline"

# Observable outcome
"Form submits without page reload"
"Error clears when user starts typing again"
"Password field value is never logged or stored in plaintext"

# Testable assertion
"Unit test: middleware rejects expired tokens with 401"
"E2E test: login → dashboard redirect works"
"Performance: form renders in < 100ms"
```

**Bad AC (avoid):**
```
"Works correctly"           # How do we know?
"Is tested"                 # What tests? What coverage?
"Follows best practices"    # Which ones?
"Is fast"                   # How fast?
```

## Roadmap-Level Planning

For strategic planning, create epics first, then flesh out features:

```
# Phase 1: Define the epics (strategic layer)
auth_epic = task_create(title="User Auth", issue_type="epic", ...)
payments_epic = task_create(title="Payments", issue_type="epic", ...)
onboarding_epic = task_create(title="Onboarding", issue_type="epic", ...)

# Block epics on each other if needed
task_blockers_add(id=payments_epic, blocking_id=auth_epic, ...)  # Payments needs auth first

# Phase 2: Create features for the first epic
task_create(title="Login UI", issue_type="feature", parent=auth_epic, ...)
task_create(title="Registration", issue_type="feature", parent=auth_epic, ...)

# Phase 3: SM/agent breaks features into tasks before execution
task_create(title="JWT middleware", issue_type="task", parent=login_feature, ...)
```

## Linking Memory to Work

Always connect work items to relevant architectural knowledge:

```
# After writing an ADR
task_update(
  id="feature-id",
  project="...",
  memory_refs_add=["decisions/adr-005-jwt-session.md"]
)

# In task design field, reference memory notes
design="""
Follow ADR-005 (stored in djinn memory: decisions/adr-005-jwt-session.md).
Use pattern from memory: patterns/express-middleware.md.
"""
```

This creates bidirectional links: tasks reference memory, memory can look up tasks.

## Decomposition Checklist

Before submitting a feature for execution:

- [ ] Title is imperative and specific ("Add login form", not "Login work")
- [ ] `description` has context and user value, NOT implementation details
- [ ] `design` has exact implementation approach, file references, ADR refs
- [ ] `acceptance_criteria` has observable, testable outcomes
- [ ] Sized to complete in one session (2-4h for features, < 1h for tasks)
- [ ] Blockers set if it depends on unreleased work
- [ ] Labels added for sprint, area, and any cross-cutting concerns
- [ ] Memory refs linked if ADRs or patterns apply
- [ ] Priority set (0=must-ship-now, 1=important, 2=nice-to-have)
