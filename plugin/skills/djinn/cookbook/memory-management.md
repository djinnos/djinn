# Memory Management Cookbook

Complete guide to organizing, creating, and retrieving knowledge in djinn's knowledge base.

## The Knowledge Base at a Glance

Memory is a wiki-style knowledge base stored as markdown files. Notes are:
- **Typed** — type determines the folder and gives semantic meaning
- **Connected** — `[[wikilinks]]` create navigable relationships
- **Searchable** — full-text search with BM25 ranking
- **Versioned** — backed by git, with full history

### Note Types and Folders

| Type | Folder | Use For |
|------|--------|---------|
| `adr` | `decisions/` | Architecture Decision Records |
| `pattern` | `patterns/` | Reusable code/design patterns |
| `research` | `research/` | Analysis, market research, findings |
| `session` | `research/sessions/` | Session notes, discoveries |
| `competitive` | `research/competitive/` | Competitive analysis |
| `tech_spike` | `research/technical/` | Technical investigation results |
| `requirement` | `requirements/` | PRDs, specs, requirements |
| `reference` | `reference/` | External docs, standards, glossary |
| `design` | `design/` | System design documents |
| `persona` | `design/personas/` | User personas |
| `journey` | `design/journeys/` | User journey maps |
| `design_spec` | `design/specs/` | UI/UX specifications |
| `brief` | (root) | Project brief — ONE per project |
| `roadmap` | (root) | Product roadmap — ONE per project |

## Orient Yourself First

**Always run this at the start of a new session:**

```
memory_catalog()
```

Returns the full table of contents — tells you what knowledge exists before creating duplicates.

## Creating Notes

### Architecture Decision Record (ADR)
```
memory_write(
  title="ADR-001: Use JWT for Authentication",
  type="adr",
  content="""
# ADR-001: Use JWT for Authentication

## Status
Accepted

## Context
We need stateless authentication that works across microservices.

## Decision
Use JWT tokens with RS256 signing. Refresh tokens rotate on use.

## Consequences
- **Good**: Stateless, scalable, cross-service
- **Bad**: Token revocation requires blocklist
- **Mitigation**: Short expiry (15min) + refresh rotation

## Relations
- [[Auth Service Design]]
- [[Session Management Pattern]]
""",
  tags=["auth", "security", "jwt"]
)
```

### Pattern
```
memory_write(
  title="Repository Pattern",
  type="pattern",
  content="""
# Repository Pattern

## Problem
Direct database calls scattered throughout business logic creates coupling.

## Solution
Wrap data access in repository interfaces. Business logic depends on interfaces, not implementations.

## Implementation
```go
type UserRepository interface {
    FindByID(ctx context.Context, id string) (*User, error)
    Save(ctx context.Context, user *User) error
}
```

## When to Use
- Complex domain with multiple data sources
- Need for testability (mock repositories in tests)
- Domain logic shouldn't know about persistence

## Relations
- [[ADR-002: Database Strategy]]
- [[Domain Driven Design]]
""",
  tags=["architecture", "data-access", "go"]
)
```

### Research Note
```
memory_write(
  title="Competitor Analysis: Auth Solutions",
  type="research",
  content="""
# Competitor Analysis: Auth Solutions

## Findings

Auth0 dominates the market but expensive at scale.
Clerk is gaining traction with better DX.
Firebase Auth works well for single-provider.

## Recommendation

Self-hosted with JWT for cost control. Consider Clerk if DX becomes priority.

## Relations
- [[ADR-001: Use JWT for Authentication]]
""",
  tags=["auth", "research", "competitors"]
)
```

### Project Brief (singleton — one per project)
```
memory_write(
  title="Project Brief",
  type="brief",
  content="""
# Project Brief

## Vision
[One paragraph]

## Problem
[What we're solving]

## Target Users
- [[Persona: Power Developer]]
- [[Persona: Team Lead]]

## Success Metrics
- [Metric 1]

## Constraints
- [Technical constraints]
- [Business constraints]
"""
)
```

## Wikilinks

Wikilinks (`[[Note Title]]`) are the core of the knowledge graph. They:
- Create navigable relationships between notes
- Show up in the knowledge graph visualization
- Enable `memory_build_context()` to traverse related content

**Always add a Relations section:**
```markdown
## Relations
- [[ADR-001: JWT Auth]]        # Explicit link
- [[Pattern: Repository]]      # Related pattern
- [[Brief: Project X]]         # Parent context
```

**Link from tasks to memory:**
```
task_update(
  id="task-id",
  project="...",
  memory_refs_add=["decisions/adr-001-jwt-auth.md"]
)
```

## Searching Memory

### Full-text search
```
# Search everywhere
memory_search(query="JWT authentication token")

# Search in a specific folder
memory_search(query="database connection", folder="decisions")

# Search by type
memory_search(query="auth", type="adr")

# Limit results
memory_search(query="performance", limit=5)
```

### List by folder
```
# All decisions
memory_list(folder="decisions")

# Go deeper
memory_list(folder="decisions", depth=2)

# Unlimited depth
memory_list(folder="research", depth=0)
```

### Recent notes
```
# Last 7 days (default)
memory_recent()

# Last 24 hours
memory_recent(timeframe="24h", limit=5)

# This week
memory_recent(timeframe="today", project="...")
```

### Build context (follow links)
```
# Get a note + all notes it links to
memory_build_context(
  url="decisions/adr-001-jwt-auth.md",
  depth=2,           # Follow 2 levels of links
  max_related=10     # Cap related notes
)
```

## Editing Notes

### Append content
```
memory_edit(
  identifier="ADR-001: Use JWT for Authentication",
  operation="append",
  content="""

## Update (2026-03-01)
Added token rotation after security review. See [[Security Audit Q1]].
"""
)
```

### Prepend content
```
memory_edit(
  identifier="some-note",
  operation="prepend",
  content="## Status\nSuperseded by [[ADR-007]]\n\n"
)
```

### Replace a section
```
memory_edit(
  identifier="ADR-001: Use JWT for Authentication",
  operation="replace_section",
  section="Consequences",
  content="""## Consequences
- **Good**: Stateless, scalable, cross-service
- **Good**: Refresh rotation limits exposure window
- **Bad**: Requires blocklist for immediate revocation
- **Mitigation**: 15min expiry + rotation
"""
)
```

### Find and replace text
```
memory_edit(
  identifier="Auth Service Design",
  operation="find_replace",
  find_text="bcrypt with cost 10",
  content="bcrypt with cost 12 (see security review)"
)
```

## Large Documents

For documents over 150 lines, build them incrementally:

```
# Step 1: Create with initial content
memory_write(
  title="System Architecture Overview",
  type="design",
  content="# System Architecture Overview\n\n## Overview\n[Initial section]"
)

# Step 2: Append remaining sections
memory_edit(
  identifier="System Architecture Overview",
  operation="append",
  content="\n## Services\n[Services section content]"
)

memory_edit(
  identifier="System Architecture Overview",
  operation="append",
  content="\n## Data Flow\n[Data flow content]"
)
```

## Maintenance

### Check health
```
memory_health()    # Total notes, broken links, orphans, stale notes
```

### Find orphaned notes (no inbound links)
```
memory_orphans()
memory_orphans(folder="decisions")  # Scope to folder
```

### Fix broken wikilinks
```
memory_broken_links()    # Find all broken [[links]]
memory_broken_links(folder="research")
```

### Move a note
```
memory_move(
  identifier="old-title-or-permalink",
  destination="decisions/new-location.md"
)
```

### Delete a note
```
memory_delete(identifier="Outdated Research Note")
```

## Knowledge Base Patterns

### Start a session
```
# 1. Orient
memory_catalog()

# 2. Find relevant context
memory_search(query="topic you're working on")

# 3. Build deep context if needed
memory_build_context(url="decisions/relevant-adr.md", depth=2)
```

### After making an architectural decision
```
# 1. Write the ADR
memory_write(title="ADR-N: Decision Title", type="adr", content="...")

# 2. Link related notes
memory_edit(
  identifier="ADR-N: Decision Title",
  operation="append",
  content="\n## Relations\n- [[Related ADR]]\n- [[Affected Pattern]]"
)

# 3. Link from task
task_update(id="task-id", project="...", memory_refs_add=["decisions/adr-n.md"])
```

### After completing research
```
# 1. Write research note
memory_write(title="Research: Topic", type="research", content="# Findings\n...")

# 2. Update roadmap/brief if it changes direction
memory_edit(identifier="roadmap", operation="find_replace", ...)

# 3. Link from relevant tasks
task_update(id="...", memory_refs_add=["research/research-topic.md"])
```

### Finding notes linked to a task
```
task_memory_refs(id="task-id", project="...")
```

### Finding tasks that reference a memory note
```
memory_task_refs(permalink="decisions/adr-001.md", project="...")
```
