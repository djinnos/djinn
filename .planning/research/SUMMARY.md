# Project Research Summary

**Project:** Djinn Planning System (GSD Fork)
**Domain:** AI agent planning system with MCP integration and multi-runtime skill distribution
**Researched:** 2026-03-02
**Confidence:** HIGH

## Executive Summary

This project forks GSD's proven planning methodology (deep questioning, parallel research, structured requirements, phased roadmaps) and adapts it for Djinn's MCP-based memory and task systems. The core architectural move is replacing GSD's filesystem storage (`.planning/` directory, STATE.md, 11 CJS modules) with direct MCP tool calls to Djinn's memory, task, and execution APIs. This is not a reimplementation -- it is a storage adapter swap on top of a methodology that has been validated at scale (34 workflows, 23-plan projects in the wild). The Agent Skills specification (agentskills.io, adopted by 26+ platforms) makes multi-runtime distribution dramatically simpler than GSD's original per-runtime conversion approach, collapsing ~400 lines of frontmatter conversion code into zero.

The recommended approach is to port methodology first, mechanism second. Each GSD workflow's questioning depth, revision loops, and research structure must survive the port verbatim -- only the output destinations change (memory_write instead of file write, task_create instead of PLAN.md). The system decomposes into four SKILL.md-based workflows (new-project, plan-phase, discuss-phase, progress) backed by seven subagent definitions (4 researchers, synthesizer, roadmapper, planner + plan-checker), distributed as a Claude Code plugin and an NPM installer for OpenCode, Gemini, and Codex.

The key risks are: (1) recreating GSD's filesystem state machine inside Djinn memory notes instead of deriving state from the live task board, (2) losing methodology quality while focusing on MCP plumbing, and (3) an unresolved hierarchy mapping ambiguity (phases as epics vs. phases as features) that will infect every workflow if not locked down before implementation begins. All three are preventable with upfront architecture decisions and disciplined porting practices.

## Key Findings

### Recommended Stack

The stack is almost entirely markdown prompt engineering backed by MCP -- there is very little "code" in the traditional sense. Skills are SKILL.md files following the Agent Skills specification. Workflows are prompt documents with numbered steps. Subagents are focused prompt templates. The only actual code is the NPM installer (~200-300 lines of Node.js) and optionally hook scripts.

**Core technologies:**
- **Agent Skills (SKILL.md)**: Universal skill format adopted by all 4 target runtimes -- eliminates per-runtime frontmatter conversion entirely
- **MCP stdio transport**: All runtimes support stdio MCP natively; Djinn already uses `djinn-server --mcp-connect`
- **Claude Code Plugin format**: First-class distribution for Claude Code users (plugin.json + skills/ + hooks/ + .mcp.json)
- **NPM package (bin installer)**: Distribution for non-Claude runtimes; simplified from GSD's ~1200 lines to ~200-300 lines
- **Markdown prompt engineering**: Workflows are prompt documents, not code; the LLM is the execution engine

**Critical version requirements:**
- Node >=16.7 for NPM installer (built-in modules only, zero external deps)
- MCP spec 2025-11-25 (stdio transport)
- Agent Skills spec v1 (agentskills.io)

### Expected Features

**Must have (table stakes):**
- Deep questioning / context gathering (discuss-phase methodology, port verbatim from GSD)
- Parallel research with 4 dimension agents + synthesizer (the core of informed planning)
- Requirements definition with REQ-IDs and traceability
- Phased roadmap generation (phases as Djinn features under a milestone epic)
- Phase planning that creates Djinn tasks with acceptance criteria, design fields, and blocker dependencies
- Progress awareness and routing (query Djinn state, route to next action)
- Multi-runtime support (Claude Code plugin + NPM installer)
- Wave-based task ordering via blocker dependencies
- Work decomposition guidance (epic > feature > task sizing)
- Project brief creation and persistence

**Should have (competitive differentiators):**
- MCP-native storage with zero import step (eliminates the GSD-Djinn bridge)
- Persistent knowledge base across sessions (research from months ago is instantly findable)
- Bidirectional memory-task linking (trace why a decision was made)
- Integrated execution pipeline (plan-to-running-code in one system)
- Context-efficient orchestration (MCP calls for state, not file reads consuming context)
- Research persistence with knowledge graph (wikilinks accumulate institutional knowledge)

**Defer (v1.x / v2+):**
- Milestone lifecycle management (complete-milestone, new-milestone)
- Quick task mode / ad-hoc tasks outside phase structure
- Cross-project knowledge reuse
- Research revision loop (plan-checker 3-iteration loop) -- consider for v1 if time permits
- PRD express path, brownfield codebase mapping
- Audit/health workflows, administrative phase manipulation, test planning

### Architecture Approach

The system is a four-layer architecture: user entry points (slash commands), workflow orchestrators (multi-step flows), focused subagents (content creation), and Djinn MCP (storage/execution). Agents call MCP directly (Option A from the research) -- there is no intermediate adapter layer because agents are prompts that instruct the LLM to call tools, and swapping `Write` for `memory_write` is a direct substitution. The filesystem-to-MCP artifact mapping is comprehensive: PROJECT.md becomes memory type=brief, REQUIREMENTS.md becomes type=requirement, ROADMAP.md becomes type=roadmap + task_create for epics/features, research files become type=research with dimension tags, STATE.md is eliminated entirely (task board IS the state), and phase directories are replaced by task hierarchy + labels.

**Major components:**
1. **Workflow Orchestrators** (4): new-project, plan-phase, discuss-phase, progress -- own the flow, not the content
2. **Subagents** (7): 4 researchers, synthesizer, roadmapper, planner (+ plan-checker) -- own content creation, write directly to MCP
3. **Claude Code Plugin**: Plugin distribution with skills/, hooks/, .mcp.json -- existing structure extended with planning workflows
4. **NPM Installer**: Multi-runtime distribution for OpenCode, Gemini, Codex -- file copy + config merge, no content transformation

### Critical Pitfalls

1. **Ghost State** -- Recreating GSD's STATE.md as a Djinn memory note instead of deriving state from live task/execution queries. Prevention: never store "current phase" or "current plan" in memory; always query `task_list(status=...)` and `execution_phase_list()`.

2. **Hierarchy Mapping Ambiguity** -- PROJECT.md contains contradictory mappings (phases as epics vs. phases as features). Prevention: resolve in an ADR before any workflow code. Recommendation: milestone=epic, phase=feature, plan=task (matches Djinn's native hierarchy).

3. **Methodology Loss During Porting** -- Focusing on MCP plumbing while diluting questioning depth, revision loops, and research structure. Prevention: port methodology first, then swap storage calls. Compare each ported workflow against GSD original for methodology completeness.

4. **MCP Partial Failure** -- Sequential task creation without idempotency leaves inconsistent state on connection errors. Prevention: use `execution_apply_changes` for batch mutations; every workflow starts with a resume check.

5. **Prompt Size Explosion** -- Inlining MCP tool documentation into already-large workflow templates. Prevention: enforce 600-line budget per workflow; reference cookbooks instead of inlining; each workflow declares its tool subset (5-10 tools, not 70+).

## Implications for Roadmap

Based on research, suggested phase structure:

### Phase 0: Architecture Decisions

**Rationale:** Two critical ambiguities must be resolved before any workflow code is written -- the hierarchy mapping (phases as epics vs. features) and the state derivation principle (no stored state). These are load-bearing decisions that every subsequent phase depends on.
**Delivers:** Two ADRs stored in Djinn memory; updated PROJECT.md with resolved contradictions; the artifact mapping table as a reference note
**Addresses:** Hierarchy mapping ambiguity (Pitfall 6), Ghost State prevention (Pitfall 1)
**Avoids:** Inconsistent hierarchy across workflows, redundant state tracking
**Research needed:** No -- this is decision-making from existing research, not new research

### Phase 1: Skill Scaffolding and Foundation Patterns

**Rationale:** The SKILL.md files and their references/ directories must exist before methodology can be ported. This phase establishes the file layout, the progressive disclosure pattern (metadata -> instructions -> resources), and the context-loading pattern (memory_read/search replacing file reads). It also creates the shared MCP adapter patterns that all workflows will reference.
**Delivers:** Skill directory structure; SKILL.md stubs for all 4 workflows; shared references (MCP adapter patterns, output templates); updated plugin.json
**Addresses:** MCP tool explosion (Pitfall 3) via tool subset declarations; Prompt size explosion (Pitfall 7) via cookbook extraction
**Uses:** Agent Skills spec, Claude Code Plugin format
**Research needed:** No -- well-documented patterns from Agent Skills spec and existing plugin structure

### Phase 2: Core Workflow -- new-project

**Rationale:** new-project is the entry point for the entire system. It produces the brief, research, requirements, and roadmap that all other workflows consume. Without it, nothing else can run. This is also the most methodology-heavy workflow (deep questioning, 4 parallel researchers, synthesizer, requirements definition, roadmap creation).
**Delivers:** Working /djinn:new-project that produces brief, research notes, requirements, and roadmap in Djinn memory, plus epics/features on the task board
**Addresses:** Deep questioning, parallel research, requirements definition, roadmap generation, MCP-native storage
**Avoids:** Methodology loss (Pitfall 4) -- this phase must be validated against GSD's original questioning depth and research quality
**Implements:** Workflow orchestrator + all 6 subagents (4 researchers, synthesizer, roadmapper)
**Research needed:** Yes -- the parallel research spawning mechanism (how to coordinate 4 dimension agents writing to Djinn memory without conflicts) needs validation during planning

### Phase 3: Core Workflow -- plan-phase

**Rationale:** Once new-project creates a roadmap and epics, plan-phase is the next step in the core loop. It is the most complex port -- orchestrating phase-researcher, planner, and plan-checker to produce Djinn tasks with structured fields (acceptance_criteria, design, blocker dependencies). This phase depends on Phase 2 outputs existing.
**Delivers:** Working /djinn:plan-phase that reads roadmap/requirements, researches phase context, creates tasks under the phase feature with wave ordering
**Addresses:** Phase planning, wave-based ordering, work decomposition, acceptance criteria
**Avoids:** Identity mismatch (Pitfall 2) -- tasks must use Djinn IDs and labels, not filesystem path conventions
**Implements:** Planner subagent, plan-checker subagent, phase-researcher subagent
**Research needed:** Yes -- the plan-checker revision loop (3 iterations in GSD) and how it maps to Djinn's task_comment_add for feedback needs design attention

### Phase 4: Supporting Workflows -- discuss-phase and progress

**Rationale:** discuss-phase enriches planning quality by capturing gray areas, design decisions, and scope boundaries before plan-phase runs. progress provides the routing that tells users what to do next. Both are simpler than the core workflows and depend on Phases 2-3 being functional.
**Delivers:** Working /djinn:discuss-phase (design notes in memory) and /djinn:progress (state queries with routing)
**Addresses:** Deep questioning for specific phases, progress awareness and routing
**Avoids:** Ghost state (Pitfall 1) -- progress must derive state from task queries, not stored state notes
**Research needed:** No -- discuss-phase is a straightforward methodology port; progress is pure MCP queries

### Phase 5: Multi-Runtime Distribution

**Rationale:** Packaging comes last. The workflows must work before they can be distributed. Claude Code users already get the plugin; this phase adds the NPM installer for OpenCode, Gemini, and Codex.
**Delivers:** NPM package with multi-runtime installer; tested installation on all 4 target runtimes; MCP configuration per runtime
**Addresses:** Multi-runtime support, NPM distribution
**Uses:** Agent Skills spec (no content transformation needed), MCP config per runtime
**Research needed:** No -- GSD's installer pattern is well-understood; the simplification (no frontmatter conversion) makes it straightforward

### Phase Ordering Rationale

- Phase 0 before everything: hierarchy and state decisions are load-bearing and cannot be changed later without re-creating all tasks
- Phase 1 before workflows: the skill scaffolding and shared patterns must exist before methodology is ported into them
- Phase 2 before Phase 3: new-project produces the artifacts (roadmap, requirements, research) that plan-phase consumes
- Phase 4 after Phases 2-3: supporting workflows enhance the core loop but are not blocking
- Phase 5 last: distribution is packaging, not functionality; test on Claude Code during development, package for other runtimes at the end
- The dependency chain is strictly linear: 0 -> 1 -> 2 -> 3 -> 4 -> 5 (though 4 and 5 could potentially run in parallel)

### Research Flags

Phases likely needing deeper research during planning:
- **Phase 2 (new-project):** Parallel research agent coordination -- how 4 agents write to Djinn memory simultaneously without conflicts; how the synthesizer knows when all 4 are complete
- **Phase 3 (plan-phase):** Plan-checker revision loop design -- how to implement the 3-iteration quality check using task comments and re-planning

Phases with standard patterns (skip research-phase):
- **Phase 0 (Architecture Decisions):** Pure decision-making from existing research
- **Phase 1 (Skill Scaffolding):** Follows Agent Skills spec directly
- **Phase 4 (Supporting Workflows):** Straightforward methodology port + MCP query patterns
- **Phase 5 (Distribution):** Simplified version of GSD's proven installer pattern

## Confidence Assessment

| Area | Confidence | Notes |
|------|------------|-------|
| Stack | HIGH | Agent Skills spec is official and adopted by all target runtimes. MCP stdio is proven. Direct access to both GSD source and Djinn codebase. |
| Features | HIGH | Direct access to GSD source (34 workflows), Djinn MCP tool API, and competitor analysis (Kiro, BMAD, cc-sdd). Feature landscape is well-mapped. |
| Architecture | HIGH | The filesystem-to-MCP translation is mechanical and well-understood. Option A (agents call MCP directly) is the obvious choice. Data flows are clear. |
| Pitfalls | HIGH | Pitfalls derived from direct code analysis (680 lines of state.cjs, 44 filesystem calls in phase.cjs) plus MCP integration guides from industry sources. |

**Overall confidence:** HIGH

The high confidence is justified because both the source system (GSD) and the target system (Djinn) are fully accessible for direct code inspection. This is not a greenfield architecture -- it is a well-scoped adaptation of proven patterns to a known target. The primary uncertainty is in the parallel agent coordination mechanism (Phase 2) and the plan-checker revision loop (Phase 3), which are the most complex orchestration patterns in the system.

### Gaps to Address

- **Parallel agent coordination mechanism:** How do 4 dimension researchers write to Djinn memory in parallel? Does Djinn's execution engine handle this natively, or does the new-project orchestrator need to poll for completion? Validate during Phase 2 planning.
- **Hierarchy mapping resolution:** PROJECT.md has a contradictory Key Decisions entry ("Phases -> Epics not Features") versus the task hierarchy table (Phase -> Feature). The research recommends milestone=epic, phase=feature, plan=task. This must be resolved as an ADR in Phase 0.
- **Plan-checker loop design:** GSD's plan-checker runs up to 3 revision iterations. How this maps to Djinn task comments and re-planning needs design work during Phase 3 planning.
- **Upstream compatibility strategy:** The constraint says "fork structure should allow pulling upstream methodology improvements." How to structure the fork so methodology updates from GSD can be merged is not yet designed. Address during Phase 1 when establishing the skill directory layout.
- **MCP batch operations:** Whether `execution_apply_changes` supports the full set of task mutations needed during plan-phase (create + set blockers + set labels in one call) needs verification.

## Sources

### Primary (HIGH confidence)
- GSD v1.22.0 source at `/home/fernando/git/references/get-shit-done/` -- 34 workflows, 11 agents, 11 CJS modules, installer
- Djinn plugin at `/home/fernando/git/djinn/plugin/` -- SKILL.md, 6 cookbooks, MCP tool API, existing plugin structure
- Djinn PROJECT.md -- project requirements, constraints, key decisions
- [Agent Skills Specification](https://agentskills.io/specification) -- SKILL.md format, progressive disclosure, cross-platform standard
- [MCP Specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) -- Protocol spec, stdio transport
- [Claude Code Plugin Documentation](https://code.claude.com/docs/en/plugins) -- Plugin structure, distribution
- [Claude Code Skills Documentation](https://code.claude.com/docs/en/skills) -- Skill format, slash commands
- [OpenAI Codex Skills](https://developers.openai.com/codex/skills/) / [Codex MCP](https://developers.openai.com/codex/mcp/) -- Codex integration
- [Gemini CLI MCP Servers](https://geminicli.com/docs/tools/mcp-server/) -- Gemini MCP configuration
- [OpenCode MCP Servers](https://opencode.ai/docs/mcp-servers/) / [OpenCode Agents](https://opencode.ai/docs/agents/) -- OpenCode integration

### Secondary (MEDIUM confidence)
- [Kiro spec-driven development](https://kiro.dev/) -- Competitor analysis (EARS notation, agent hooks)
- [BMAD Method](https://github.com/bmad-code-org/BMAD-METHOD) -- Competitor analysis (26 agents, 68 workflows)
- [MCP Pitfalls -- HiddenLayer](https://hiddenlayer.com/innovation-hub/mcp-model-context-pitfalls-in-an-agentic-world/) -- MCP security/integration patterns
- [Implementing MCP -- Nearform](https://nearform.com/digital-community/implementing-model-context-protocol-mcp-tips-tricks-and-pitfalls/) -- MCP integration tips
- [Gemini CLI Agent Skills](https://medium.com/google-cloud/beyond-prompt-engineering-using-agent-skills-in-gemini-cli-04d9af3cda21) -- Gemini skill adoption

### Tertiary (LOW confidence)
- [GSD community coverage](https://ccforeveryone.com/gsd) -- Real-world usage patterns, anecdotal (needs validation)

---
*Research completed: 2026-03-02*
*Ready for roadmap: yes*
