---
title: "ADR-038: Configurable Agent Roles, Domain Specialists, and Auto-Improvement"
type: adr
tags: ["adr","architecture","agents","roles","specialists","auto-improvement","paperclip","autoresearch"]
---



# ADR-038: Configurable Agent Roles, Domain Specialists, and Auto-Improvement

## Status: Draft

Date: 2026-03-19

Extends: ADR-034 (Agent Role Hierarchy), ADR-023 (Cognitive Memory Architecture)

## Context

### The Fixed-Role Limitation

ADR-034 defines six agent roles (Worker, Lead, Planner, Architect, Reviewer, Resolver) with hardcoded capabilities, tool policies, and system prompts baked into Rust code. This works for the general case but creates problems:

1. **Agents don't learn from experience** — a Worker that has completed 50 database migration tasks makes the same mistakes as one on its first. Session reflections (ADR-023 §7) capture knowledge into memory, but that knowledge isn't fed back into the agent's behavior.

2. **No domain specialization** — all Workers are identical. A task touching database migrations gets the same agent as one touching MCP tool handlers, despite requiring completely different expertise, tools, and verification strategies.

3. **Planner can't route intelligently** — with identical Workers, the Planner has no basis for assigning tasks to the best-fit agent. It assigns work round-robin or by availability, not by capability.

4. **Users can't extend agents** — users who know their codebase intimately have no way to inject that knowledge into agent behavior beyond writing memory notes and hoping `build_context` surfaces them.

### Reference Implementations

**Paperclip** (AI company orchestration platform) treats agents as configurable data: adapter type + config blob + injected skills + org position. Agents are runtime-agnostic shells whose capabilities come from their configuration, not from code. Skills are markdown files injected at runtime. Users create agents by picking an adapter, writing instructions, and assigning skills. The platform doesn't prescribe agent identity — adapters do.

**Autoresearch** (autonomous ML research) demonstrates a tight improvement loop: propose change → execute → evaluate with deterministic metric → keep or discard. Applied to `train.py` hyperparameters, but the pattern generalizes: any configuration that can be evaluated against an outcome metric can be iteratively improved.

### What ADR-023 Already Provides

Session reflection (§7) extracts cases, patterns, and pitfalls from completed sessions. Confidence scoring (§4) tracks which knowledge is reliable based on task outcomes. Implicit associations (§3) learn which notes are useful together. `build_context` with progressive disclosure (§2, §9) compresses this into agent-consumable context.

The learning infrastructure exists. What's missing is the **feedback path**: taking what the memory system learned and feeding it back into the agent's configuration.

## Decision

### 1. Roles as Data, Not Code

Extract the current hardcoded role definitions into a configurable representation. Each role becomes a data entity with:

```
AgentRole:
  name: String                    # "Worker", "DB Expert", etc.
  base_role: BaseRole             # Worker | Lead | Planner | Architect | Reviewer | Resolver
  description: String             # What this agent specializes in (used by Planner for routing)
  mcp_servers: Vec<McpServerRef>  # Additional MCP servers this agent can use
  skills: Vec<SkillRef>           # Skills (prompt templates) available to this agent
  system_prompt_extensions: Text  # Additional system prompt content appended to base role prompt
  model_preference: Option<String># Preferred model (falls back to project default)
  verification_command: Option<String> # Custom verification (falls back to project default)
  is_default: bool                # Whether this is the default for its base_role
```

**Base roles define the behavioral contract** (ADR-034):
- **Worker**: full lifecycle, can read/write/edit code, runs verification
- **Lead**: unblocks workers, triages stuck tasks, 10-min timeout
- **Planner**: decomposes epics into tasks, manages roadmap, no code editing
- **Architect**: strategic review, health patrol, can read code but not edit
- **Reviewer**: reviews completed work
- **Resolver**: resolves merge conflicts

**Specialists extend a base role** with domain knowledge and tools. A "DB Expert" is a Worker with extra prompt content about migrations, a postgres MCP server, and a migration-safety skill. It inherits all Worker behavior (lifecycle, tool policy, verification flow, escalation rules).

### 2. Default Instances

Each base role has exactly one **default** instance. Out of the box, Djinn works identically to ADR-034 — six default roles, no configuration required.

Users create specialists by cloning a base role and adding extensions. If no specialist matches a task, the default instance handles it.

### 3. Planner Routes by Capability, Not by Path

When the Planner decomposes an epic into tasks, it sees available specialists:

```
Available Workers:
  - Worker (default): General-purpose code changes
  - DB Expert: Database migrations, schema changes, query optimization
  - MCP Specialist: MCP tool handlers, JSON-RPC, rmcp
```

The Planner assigns based on **task semantics** — "this task is about adding a new migration" → DB Expert. No path matching, no filesystem coupling. The Planner reads each specialist's name + description and picks the best fit, falling back to the default Worker for anything that doesn't match.

This is an LLM decision, not a rule. The Planner's prompt includes the specialist roster with descriptions. Cost is negligible — it's one extra paragraph in the decomposition prompt.

For non-Worker roles: same pattern. If the user creates a "Frontend Planner" that knows React decomposition patterns, the system uses it for UI-related epics. Default Planner handles everything else.

### 4. Auto-Improvement via Architect Patrol (Autoresearch Pattern)

The Architect patrol (ADR-034, every 5 minutes) already monitors board health. Extend it to monitor **agent effectiveness** and propose configuration improvements.

#### The Feedback Loop

```
1. Agent completes tasks → session reflection (ADR-023 §7) fires
   - Extracts cases, patterns, pitfalls
   - Updates confidence scores
   - Records co-access associations

2. Knowledge accumulates in memory per domain:
   - "Migrations need --reversible flag" (case, confidence 0.85)
   - "Always check FK constraints before DROP" (pitfall, confidence 0.92)
   - "Full workspace test is wasteful for single-crate changes" (pattern, confidence 0.78)

3. Architect patrol reviews agent outcomes:
   - Success rate per specialist (rolling window)
   - Token usage per task (efficiency)
   - Common failure patterns (from pitfall notes)
   - Time-to-complete trends

4. Architect calls build_context(intent="agent:{role_name} effectiveness")
   - Gets back compressed cases/patterns/pitfalls relevant to that specialist
   - ADR-023's tiered disclosure ensures Architect isn't flooded

5. Architect proposes system_prompt_extensions amendment:
   - Based on accumulated knowledge, not guesswork
   - Example: "When running migrations, always use --reversible flag"
   - Append-only: new learnings are added, not replacing user content

6. Evaluation (next N tasks):
   - Did success rate improve? → Keep amendment
   - No improvement or regression? → Discard
   - Log outcome to agent improvement history
```

#### Separation of Concerns

- **User-written extensions**: The user's system_prompt_extensions are never modified by auto-improvement. They represent intentional configuration.
- **Learned extensions**: Auto-improvement appends to a separate `learned_prompt` field. Users can review, edit, or clear learned content.
- **Memory notes**: The raw cases/patterns/pitfalls remain in the knowledge base. Learned prompt is a distilled summary of what's proven useful.

#### Evaluation Metrics

Unlike autoresearch's clean `val_bpb`, agent effectiveness is multi-dimensional:

| Metric | Signal | Weight |
|--------|--------|--------|
| Task success rate | Did the task close without force-close or excessive reopens? | High |
| Token efficiency | Tokens used per successful task completion | Medium |
| Time-to-complete | Wall clock from assignment to close | Medium |
| Verification pass rate | First-attempt verification success | Medium |
| Reopen count | How often did the task bounce back? | High (inverse) |

Metrics are tracked per specialist over a rolling window (last 20 tasks or 7 days, whichever is larger). Improvement is measured as delta between windows.

### 5. User Configuration Surface

Users interact with specialists through:

1. **MCP tools**: `role_create`, `role_update`, `role_list`, `role_show` — CRUD for specialist definitions
2. **Chat**: "Create a DB expert agent that knows about our migration patterns" → Planner/chat agent creates the role
3. **Direct editing**: Role definitions stored as structured data (DB or config), viewable and editable

Minimal viable configuration for creating a specialist:
```
name: "DB Expert"
base_role: Worker
description: "Database migrations, schema changes, rusqlite queries"
system_prompt_extensions: |
  You specialize in database work for this project.
  We use rusqlite with WAL mode and refinery migrations.
  Always test migrations both up and down.
```

Everything else (MCP servers, skills, model preference, verification command) is optional and additive.

### 6. Skills as Assignable Prompt Templates

Skills (already exist in `/home/fernando/git/djinnos/djinn/plugin/skills/`) become assignable to roles:

- A skill is a markdown file with frontmatter (name, description, trigger conditions)
- When assigned to a role, the skill's content is available to the agent
- Skills are loaded on-demand (agent sees name + description, loads full content when relevant)
- Users can write custom skills and assign them to specialists

This mirrors Paperclip's skills injection pattern but without the tmpdir symlink complexity — Djinn's in-process agents can load skills directly from the knowledge base or filesystem.

### 7. MCP Servers as Capability Extensions

Specialists can be assigned additional MCP servers beyond what the base role provides:

- A "DB Expert" might get a `postgres-mcp` server for direct DB introspection
- An "API Specialist" might get an `openapi-mcp` server for schema validation
- MCP servers are referenced by name; the project's MCP configuration defines available servers

The base role's MCP servers are always included. Specialist MCP servers are additive.

## Phasing

This ADR builds on ADR-023 and ADR-034. Implementation follows their completion:

### Prerequisites (in progress or planned)

| Phase | ADR | Status | What it provides |
|-------|-----|--------|-----------------|
| 17c | ADR-023 | Finishing (1 task remaining) | Confidence scoring, contradiction detection, deduplication |
| 17d | ADR-023 | Not started | Session reflection — extracts cases/patterns/pitfalls |
| 20 | ADR-036 | Not started | Structured session finalization (clean session outcomes) |
| — | ADR-034 | Not started | Role hierarchy, Architect patrol, Scrum Master rules |

### This ADR's phases

**Phase 38a: Roles as Data** (after ADR-034 lands)
- Extract hardcoded role definitions into configurable data
- Add `agent_roles` table or config structure
- Default instances for all six base roles
- Planner prompt includes specialist roster
- No auto-improvement yet — just the data model and routing

**Phase 38b: User-Created Specialists** (after 38a)
- MCP tools for role CRUD (`role_create`, `role_update`, `role_list`, `role_show`)
- Chat-based role creation ("create a DB expert")
- Skills assignment to roles
- MCP server assignment to roles
- Custom verification commands per role

**Phase 38c: Agent Metrics Collection** (after 38b, parallel with usage)
- Track success rate, token usage, time-to-complete, verification pass rate per specialist
- Rolling window aggregation
- Expose metrics via MCP tool (`role_metrics`)
- This is the "results.tsv" equivalent

**Phase 38d: Auto-Improvement Loop** (after 38c + ADR-023 17d session reflection)
- Architect patrol analyzes agent metrics
- Calls `build_context` for domain-relevant cases/patterns/pitfalls
- Proposes `learned_prompt` amendments
- Keep/discard based on metric deltas
- Agent improvement history log

### Critical path

```
ADR-023 17c (finishing)
  → ADR-023 17d (session reflection — provides learning signal)
    → ADR-034 (role hierarchy — provides Architect patrol)
      → 38a (roles as data)
        → 38b (user-created specialists)
        → 38c (metrics collection)
          → 38d (auto-improvement loop, also needs 17d)
```

ADR-036 Phase 20 (structured finalization) can run in parallel and improves the quality of session outcomes that feed into metrics.

## Consequences

**Positive:**
- Agents improve with use — the more tasks they complete, the better they get at this specific codebase
- Users can inject domain knowledge without touching Rust code
- Planner routes to best-fit agent, reducing failures and rework
- Zero-config default: works identically to ADR-034 out of the box
- Build contention naturally reduces as specialists run scoped verification
- Auto-improvement is evidence-based (autoresearch pattern), not speculative
- Session reflection (ADR-023) becomes directly actionable, not just archival
- Skills and MCP servers make agents composable without code changes

**Negative:**
- More moving parts: role data model, routing logic, metrics collection, improvement loop
- Auto-improvement could propose bad changes if metrics are noisy (mitigation: conservative keep/discard thresholds, human review of learned prompt)
- Planner routing adds LLM decision cost per task creation (mitigation: specialist roster is a short paragraph, negligible token cost)
- Risk of specialist proliferation (mitigation: suggest consolidation when specialists have overlapping descriptions)

**Risks:**
- Learned prompt could accumulate contradictory advice over time → mitigation: Architect reviews learned prompt as a whole, not just appending
- Specialists with wrong scoping could cause tasks to be assigned to ill-equipped agents → mitigation: default Worker catches everything that doesn't match, Planner fallback is always safe
- Auto-improvement feedback loop could be too slow (need N tasks before signal) → mitigation: start with user-created specialists (38b) which provide immediate value; auto-improvement (38d) is additive

## Relations

- ADR-034 — extended (roles become data-driven, Architect gains improvement responsibility)
- ADR-023 — consumed (session reflection provides learning signal, build_context provides compressed knowledge)
- ADR-036 — complementary (structured finalization improves outcome signal quality)
- ADR-035 — complementary (repo map helps specialists understand their domain boundaries)
- [[Project memory broken-link and orphan backlog triage]] — inspiration for adapter-based agent configuration
- [[Project memory broken-link and orphan backlog triage]] — inspiration for propose/evaluate/keep-discard improvement loop
