# Stack Research

**Domain:** CLI agent planning/orchestration with MCP integration and multi-runtime skill distribution
**Researched:** 2026-03-02
**Confidence:** HIGH

## Recommended Stack

### Core Technologies

| Technology | Version | Purpose | Why Recommended |
|------------|---------|---------|-----------------|
| Agent Skills (SKILL.md) | agentskills.io spec v1 | Skill definition format | Open standard adopted by 26+ platforms (Claude Code, Codex, Gemini CLI, VS Code Copilot, Cursor). Single format, every runtime reads it natively. Eliminates per-runtime frontmatter conversion -- the largest source of complexity in GSD's installer (~400 lines of conversion code become unnecessary). |
| MCP stdio transport | MCP spec 2025-11-25 | Tool interface to Djinn server | All four target runtimes support MCP stdio servers natively. Djinn already uses this (`djinn-server --mcp-connect`). Planning workflows call `memory_write`, `task_create` etc. directly -- no filesystem intermediary needed. MCP tool names are identical across all runtimes. |
| Claude Code Plugin format | plugin.json v1 | Distribution for Claude Code users | First-class distribution: `plugin.json` + skills/ + hooks/ + `.mcp.json`. Already exists in `plugin/`. Plugin install gives users skills + MCP config + session hooks in one step. |
| NPM package (bin installer) | Node >=16.7 | Distribution for non-Claude runtimes | GSD's proven pattern: `npx djinn-planning` copies skills and configures MCP for OpenCode/Gemini/Codex. Uses only Node.js built-ins (fs, path, os, readline). Zero external dependencies. |
| Markdown prompt engineering | N/A | Workflow definitions | GSD's core insight: workflows are prompt documents, not code. SKILL.md files contain the full methodology. No build step, no compilation, human-readable, version-controlled. The LLM is the execution engine -- it reads markdown and follows the instructions using available tools. |

### Supporting Libraries

| Library | Version | Purpose | When to Use |
|---------|---------|---------|-------------|
| esbuild | ^0.24.0 | Bundle hooks (if needed) | Only if Claude Code hooks require JS compilation. GSD uses this for pre-commit hooks. Likely not needed for v1 since Djinn hooks use shell commands (`djinn-server --ensure-daemon`). Defer unless needed. |
| Node.js built-in test runner | Node 18+ | Installer unit tests | `node --test` -- no framework dependency. Test the installer's config file merging and directory creation logic. |

### Development Tools

| Tool | Purpose | Notes |
|------|---------|-------|
| skills-ref CLI | Validate SKILL.md files | `skills-ref validate ./my-skill` -- checks frontmatter, naming conventions. From agentskills/agentskills repo. Run in CI to catch formatting errors. |
| djinn-server | Local MCP server for testing | Already running. Workflow skills call MCP tools against this. Test by invoking skills in each target runtime against a scratch project. |

## MCP-First Workflow Template Structure

### Pattern: SKILL.md instructions call MCP tools, not filesystem

Every workflow skill follows this pattern -- the skill body is prompt text that instructs the LLM to call Djinn MCP tools:

```markdown
---
name: djinn-new-project
description: Initialize a new project with deep questioning, research, requirements, and roadmap. Creates Djinn memory notes and tasks directly via MCP.
---

# New Project

## 1. Orient
Read existing knowledge:
- `memory_catalog()` -- see what exists
- `task_list(project=..., issue_type="epic")` -- check for existing work

## 2. Question
[Deep questioning methodology -- pure prompt content, see references/questioning.md]

## 3. Create Brief
memory_write(
  type="brief",
  title="Project Brief",
  content="[synthesized from questioning]"
)

## 4. Research (if enabled)
Create research tasks and launch parallel execution:
- `task_create(issue_type="task", title="Research: Stack", ...)` per dimension
- `execution_start()` dispatches researchers in parallel
- Each researcher calls `memory_write(type="research", ...)` for findings
- Synthesizer reads `memory_search(folder="research")` and writes summary

## 5. Requirements
memory_write(
  type="requirement",
  title="Requirements",
  content="REQ-001: ...\nREQ-002: ..."
)

## 6. Roadmap
memory_write(
  type="roadmap",
  title="Roadmap",
  content="[Phase structure with success criteria]"
)
```

### Pattern: Progressive disclosure via references/

The Agent Skills spec defines a three-tier progressive disclosure model that protects the context window:

1. **Metadata** (~30-50 tokens): `name` and `description` from frontmatter, loaded at startup for ALL skills
2. **Instructions** (<5000 tokens recommended): Full SKILL.md body, loaded when skill is activated
3. **Resources** (as needed): Files in `scripts/`, `references/`, `assets/` loaded only when required during execution

Large methodology content (questioning protocol, research dimension templates, output format templates) goes in `references/` subdirectory. The SKILL.md stays under 500 lines and says "See [questioning methodology](references/questioning.md) for the protocol." The LLM loads references on demand. This is critical because Djinn's planning workflows (especially `new-project`) are long -- GSD's `new-project.md` is 300+ lines before counting referenced files.

### Pattern: No runtime-specific content in skills

Skills reference MCP tools by their universal names (`memory_write`, `task_create`, `task_list`). They never reference runtime-specific APIs (`Task()`, `AskUserQuestion`, `spawn_agent`). Each runtime's MCP client translates MCP tool calls to its native execution. This is the key architectural difference from GSD, which authored workflows in Claude Code syntax and converted at install time.

For user interaction, skills use natural language ("Ask the user which depth level they prefer: Quick, Standard, or Comprehensive") instead of runtime-specific APIs. Each runtime handles user interaction through its own mechanism.

## Installer Pattern for All Target Runtimes

### Claude Code: Plugin distribution (primary)

```
plugin/
  .claude-plugin/plugin.json     # Plugin metadata, keywords
  skills/
    djinn-new-project/
      SKILL.md                   # /djinn:new-project
      references/
        questioning.md           # Deep questioning methodology
        templates/               # Output format templates
    djinn-plan-phase/SKILL.md    # /djinn:plan-phase
    djinn-discuss-phase/SKILL.md # /djinn:discuss-phase
    djinn-progress/SKILL.md      # /djinn:progress
  hooks/hooks.json               # SessionStart: ensure djinn-server daemon
  .mcp.json                      # Djinn MCP server config (stdio)
```

Users install with `claude plugin add djinn` or by cloning. The plugin gives them skills + MCP config + hooks in one step. Skills appear as `/djinn:*` slash commands automatically.

### NPM: `npx djinn-planning` for OpenCode, Gemini, Codex

```
package.json                     # bin: { "djinn-planning": "bin/install.js" }
bin/install.js                   # Multi-runtime installer
skills/                          # Agent Skills format SKILL.md files (identical to plugin)
```

The installer (simplified from GSD's ~1200 lines to ~200-300 lines):

1. **Detect runtime** via flags: `--opencode`, `--gemini`, `--codex`, `--all`
2. **Copy SKILL.md directories** to runtime's skill/command directory:
   - OpenCode: `~/.config/opencode/skills/` (XDG path, respects `OPENCODE_CONFIG_DIR`)
   - Gemini: `~/.gemini/skills/` (respects `GEMINI_CONFIG_DIR`)
   - Codex: `~/.codex/skills/` (respects `CODEX_HOME`)
3. **Configure MCP server** in runtime config (non-destructive merge):
   - OpenCode: `opencode.json` or `opencode.jsonc` -- `mcp.djinn` block
   - Gemini: `settings.json` -- `mcpServers.djinn` block
   - Codex: `config.toml` -- `[mcp_servers.djinn]` section
4. **Set up session hook** to ensure djinn-server daemon is running (per runtime convention)

**Key simplification vs GSD:** No tool name conversion. No frontmatter rewriting. No agent TOML generation. No slash command syntax rewriting. Agent Skills format is universal. MCP tool names are universal. The installer is pure file-copy + config-merge.

### MCP Configuration Per Runtime

| Runtime | Config File | Format |
|---------|-------------|--------|
| **Claude Code** | `.mcp.json` (in plugin) | `{"mcpServers":{"djinn":{"type":"stdio","command":"djinn-server","args":["--mcp-connect"]}}}` |
| **OpenCode** | `opencode.json` | `{"mcp":{"djinn":{"type":"local","command":["djinn-server","--mcp-connect"]}}}` |
| **Gemini CLI** | `settings.json` | `{"mcpServers":{"djinn":{"command":"djinn-server","args":["--mcp-connect"]}}}` |
| **Codex** | `config.toml` | `[mcp_servers.djinn]` with `command = "djinn-server"`, `args = ["--mcp-connect"]` |

## What GSD Patterns to Keep

### Keep: Markdown workflow templates as the core artifact

GSD's 34 workflow templates are pure prompt engineering -- XML-tagged sections (`<purpose>`, `<process>`, `<step>`), structured gates, branching logic. This is the methodology. It works because LLMs execute markdown instructions reliably. The SKILL.md format is the natural container for these.

**Adaptation:** GSD workflows reference `@~/.claude/get-shit-done/workflows/X.md` via `<execution_context>` blocks. In the fork, SKILL.md files contain the workflow directly (for small skills) or use the `references/` directory for large sub-documents per the Agent Skills spec. No `@` file references that encode absolute paths.

### Keep: Command as orchestrator, agent as worker

GSD's two-tier pattern: a slash command (e.g., `/gsd:plan-phase`) orchestrates the workflow and spawns agents (e.g., `gsd-planner`, `gsd-plan-checker`) for specialized work. This maps to Agent Skills progressive disclosure: the SKILL.md loads when the user invokes `/djinn:plan-phase`, and it instructs the LLM to create tasks and launch Djinn execution for sub-agent dispatch.

**Adaptation:** GSD uses Claude Code's `Task()` API to spawn subagents. In Djinn, sub-agent dispatch happens via `task_create` + `execution_start` (Djinn's parallel orchestrator). The skill instructs the LLM to create tasks for each research dimension and launch execution -- the MCP calls are identical regardless of runtime.

### Keep: Parallel research with dimension agents + synthesizer

The 4-researcher fan-out (stack, features, architecture, pitfalls) plus a synthesizer is GSD's most differentiated pattern. Each researcher produces focused output; the synthesizer merges with conflict resolution.

**Adaptation:** Researcher outputs go to `memory_write(type="research", tags=["stack"])` instead of `.planning/research/STACK.md`. The synthesizer reads from `memory_search(folder="research")` instead of filesystem glob. Research agents are defined as skills in `references/` that Djinn's execution system dispatches.

### Keep: Deep questioning methodology

GSD's questioning protocol (collaborative, thread-following, not checklist) is the highest-leverage part of the system. It's pure prompt content with zero code dependency. Copy directly into the `/djinn:new-project` skill's `references/questioning.md`.

### Keep: REQ-ID based requirements with traceability

Requirements with `REQ-001` identifiers that trace through to roadmap phases and task acceptance criteria. Store in `memory_write(type="requirement")` with REQ-IDs in the content body. Wikilinks (`[[REQ-001: Auth]]`) connect requirement notes to design and research notes in Djinn's knowledge graph.

### Keep: Phase-based roadmap structure

Phases with success criteria, dependencies, and scoped goals. Store as `memory_write(type="roadmap")` singleton. Phase execution creates epics with `task_create(issue_type="epic")`, features per phase, and tasks per plan.

## What GSD Patterns to Replace

### Replace: `.planning/` filesystem storage

**GSD pattern:** All artifacts (PROJECT.md, REQUIREMENTS.md, ROADMAP.md, STATE.md, research/, phases/) live in `.planning/` directory on disk. Config in `config.json`.

**Why replace:** Djinn memory is the source of truth. Filesystem storage creates a dual source of truth. The existing `cookbook/gsd.md` describes a painful manual import bridge from `.planning/` files to Djinn tasks -- this entire fork exists to eliminate that bridge.

**Replacement mapping:**
| GSD File | Djinn MCP Call |
|----------|----------------|
| `.planning/PROJECT.md` | `memory_write(type="brief")` |
| `.planning/REQUIREMENTS.md` | `memory_write(type="requirement")` |
| `.planning/ROADMAP.md` | `memory_write(type="roadmap")` |
| `.planning/research/STACK.md` | `memory_write(type="research", tags=["stack"])` |
| `.planning/config.json` | `memory_write(type="reference", title="Planning Config")` |
| `.planning/phases/N-name/PLAN.md` | `task_create(issue_type="task", ...)` |
| `.planning/STATE.md` | Not needed -- `task_list()` and `execution_status()` ARE the state |

### Replace: STATE.md progression engine

**GSD pattern:** `state.cjs` reads/writes STATE.md for phase tracking, completion status, progression logic. `gsd-tools.cjs state-snapshot` produces structured JSON from markdown parsing.

**Why replace:** Djinn tasks ARE the state. Phase status = epic/feature completion percentage. Current phase = first non-closed feature under the milestone epic. Next action = `task_ready()`. The `progress` workflow queries Djinn instead of parsing STATE.md.

**Replacement:** `task_list(status=...)`, `task_count(group_by="status")`, `execution_status()`. Zero custom state management code.

### Replace: gsd-tools.cjs utility modules (11 CJS modules)

**GSD pattern:** core.cjs, config.cjs, state.cjs, phase.cjs, roadmap.cjs, milestone.cjs, template.cjs, frontmatter.cjs, commands.cjs, init.cjs, verify.cjs -- providing structured JSON to workflows via `node gsd-tools.cjs <command>`.

**Why replace:** These modules exist because GSD stores state in markdown files that need parsing into structured data. Djinn already stores structured data -- MCP tools return JSON directly. The "parse markdown, extract JSON" layer is unnecessary when `task_list()` returns structured task objects.

**What to keep conceptually:** The `init` command pattern (pre-loading all context a workflow needs in one call) is useful for reducing round-trips. Implement it as a sequence of MCP calls at the top of each skill (the "Orient" step), not as a Node.js CLI tool.

### Replace: Per-runtime frontmatter conversion

**GSD pattern:** The installer converts Claude Code frontmatter to OpenCode format (tool name mapping, `allowed-tools` to `permission`), Gemini format (YAML array tools, strip color, escape template variables), and Codex format (skill adapter headers, TOML agent configs, `$skill-name` invocation syntax). ~400 lines of conversion code.

**Why replace:** The Agent Skills specification (agentskills.io, published Dec 2025) provides a single SKILL.md format that all runtimes read natively. Claude Code, Codex, Gemini CLI, and OpenCode all support the same frontmatter fields (`name`, `description`, optional `allowed-tools`, `metadata`). MCP tool names are identical across all runtimes. GSD's conversion layer was necessary when no cross-platform standard existed -- it is now obsolete.

**Replacement:** Write SKILL.md files once in Agent Skills format. The NPM installer copies them to the correct directory per runtime without content transformation. The only per-runtime work is MCP server configuration (different config file format per runtime) and session hooks.

### Replace: Execution and verification workflows

**GSD pattern:** `execute-phase.md`, `execute-plan.md`, `verify-work.md`, `verify-phase.md` plus `gsd-executor`, `gsd-verifier` agents.

**Why replace:** Djinn's execution engine (`execution_start`, `execution_phase_list`, review pipeline with `submit_task_review` / `task_review_approve` / `phase_review_approve`) already provides parallel task dispatch, worktree isolation, task review, and phase review. These GSD workflows are redundant with Djinn's core functionality.

**Replacement:** Not ported. Explicitly out of scope per PROJECT.md.

### Replace: Milestone and admin lifecycle workflows

**GSD pattern:** `complete-milestone.md`, `new-milestone.md`, `audit-milestone.md`, `add-phase.md`, `insert-phase.md`, `remove-phase.md`, `cleanup.md`, `pause-work.md`, `resume-project.md`.

**Why replace:** These are administrative operations on `.planning/` filesystem state. In Djinn, these are simple MCP calls: `task_transition(action="close")` for milestone completion, `execution_phase_create()` for adding phases, `execution_pause()` for pausing. No workflow template needed -- they're single tool calls.

**Replacement:** Defer to later per PROJECT.md. When needed, they'll be thin skills wrapping MCP calls, not full workflow orchestrations.

## Alternatives Considered

| Recommended | Alternative | When to Use Alternative |
|-------------|-------------|-------------------------|
| Agent Skills SKILL.md format | GSD-style per-runtime frontmatter conversion | Never for new work. The conversion approach was necessary pre-Dec 2025. Agent Skills standard makes it obsolete. |
| MCP tools as the only output interface | Filesystem writes + Djinn import bridge | Never. This is exactly the bridge pattern (cookbook/gsd.md) we're eliminating. |
| NPM installer for non-Claude runtimes | Dedicated installer per runtime | Only if a runtime breaks Agent Skills compatibility. Unlikely given ecosystem convergence. |
| Pure SKILL.md (no CJS tooling) | Keep gsd-tools.cjs for structured queries | Only if MCP round-trips prove too slow for workflow initialization. Measure first -- Djinn's SQLite backend is local, so MCP calls should be sub-millisecond. |
| Monorepo (plugin/ + NPM package share skill files) | Separate codebases for plugin and NPM | Never. Skills are identical; only the distribution wrapper differs. Build step or symlinks share the source. |
| Markdown prompts with XML tags | Code-based workflow DSL (TypeScript/Python) | Never for this project. Workflows must be portable across 4+ runtimes. Code-based workflows require a build step, runtime dependency, and sandbox compatibility per runtime. Markdown is the universal format. |

## What NOT to Use

| Avoid | Why | Use Instead |
|-------|-----|-------------|
| `.planning/` directory for any artifact | Creates dual source of truth with Djinn memory. Every artifact has a Djinn memory type. | `memory_write()` with appropriate type (brief, requirement, roadmap, research, reference) |
| STATE.md / state management code | Djinn tasks ARE the state. Parsing markdown to derive status is redundant when `task_list(status=...)` returns structured JSON. | `task_list()`, `task_count()`, `execution_status()` |
| gsd-tools.cjs Node modules | 11 modules exist to parse/manage filesystem state. With MCP tools returning structured data, the parsing layer is dead code. | Direct MCP calls in SKILL.md instructions |
| Per-runtime tool name mapping tables | Agent Skills + MCP make tool names universal. GSD's `claudeToOpencodeTools`, `claudeToGeminiTools` tables (~50 lines each) are dead code. | MCP tool names are the same on every runtime |
| Runtime-specific subagent APIs in skill content | `Task()`, `spawn_agent`, `AskUserQuestion` tie skills to a single runtime. | Djinn `execution_start()` for parallel dispatch; natural language prompts for user interaction |
| Template string variables (`$ARGUMENTS`, `{{GSD_ARGS}}`) | Different runtimes use different variable syntax. GSD's installer rewrites these per-runtime. | Natural language in skills ("Parse the user's input for phase number and flags"). LLMs handle argument parsing without templating. |
| @modelcontextprotocol/sdk in planning workflows | Planning skills are MCP clients, not servers. The runtimes already know how to call MCP tools. The SDK is for building servers. | Direct MCP tool calls in skill instructions |
| HTTP/SSE MCP transport | djinn-server is local. stdio is simpler, faster, universally supported. HTTP adds connection management and port allocation complexity. | stdio transport via `djinn-server --mcp-connect` |
| Hooks-based compilation (esbuild for hook scripts) | Djinn hooks are simple shell commands. No compilation needed for v1. | Direct shell commands in hooks.json |
| Custom DSL for workflow orchestration | Over-engineering. GSD proved that markdown with "Step 1, Step 2, Step 3" sections works. LLMs follow sequential instructions natively. | Markdown `<process>` sections with numbered steps |

## Recommended Source File Layout

```
djinn/plugin/                          # Already exists -- extend this
  .claude-plugin/plugin.json           # Plugin metadata
  skills/
    djinn/                             # Existing skill (keep)
      SKILL.md                         # Core djinn skill (task/memory/execution)
      cookbook/                         # Existing cookbooks (keep)
    djinn-new-project/                 # NEW: planning workflow
      SKILL.md                         # Questioning -> research -> requirements -> roadmap
      references/
        questioning.md                 # Deep questioning methodology (from GSD)
        output-templates.md            # Memory note format templates
    djinn-plan-phase/                  # NEW: phase planning workflow
      SKILL.md                         # Research -> plan -> create tasks
      references/
        plan-templates.md              # Plan structure templates
    djinn-discuss-phase/               # NEW: phase context gathering
      SKILL.md
    djinn-progress/                    # NEW: state check and routing
      SKILL.md
  hooks/hooks.json                     # SessionStart: djinn-server --ensure-daemon
  .mcp.json                            # Djinn MCP server config

djinn-planning/                        # NEW: separate NPM package for non-Claude runtimes
  package.json                         # bin: { "djinn-planning": "bin/install.js" }
  bin/install.js                       # Multi-runtime installer (forked from GSD, simplified)
  skills/                              # Copied/symlinked from plugin/skills/ (same SKILL.md files)
```

The plugin directory IS the source of truth for skill content. The NPM package includes the same skill files plus the installer that copies them to other runtimes' config directories.

## Version Compatibility

| Component | Compatible With | Notes |
|-----------|-----------------|-------|
| Agent Skills SKILL.md | Claude Code, Codex CLI, Gemini CLI, OpenCode, VS Code Copilot | All support agentskills.io spec. `name` + `description` frontmatter required; everything else optional. |
| MCP stdio transport | All four target runtimes | All support `{"command": "djinn-server", "args": ["--mcp-connect"]}`. Config file location and format differ per runtime. |
| Djinn MCP tools | djinn-server (current) | Tool names are stable (`memory_write`, `task_create`, `task_list`, `execution_start`, etc). |
| NPM installer | Node >=16.7 | Uses only built-in modules (fs, path, os, readline). Same requirement as GSD. |
| Claude Code plugin | plugin.json v1 | Stable format: `.claude-plugin/plugin.json` + top-level `skills/`, `hooks/`, `.mcp.json`. |

## Sources

- [Agent Skills Specification](https://agentskills.io/specification) -- SKILL.md format, progressive disclosure model, cross-platform standard. Validated Dec 2025. HIGH confidence, official spec.
- [Claude Code Plugin Documentation](https://code.claude.com/docs/en/plugins) -- Plugin structure, distribution, skills bundling. HIGH confidence, official docs.
- [Claude Code Skills Documentation](https://code.claude.com/docs/en/skills) -- Skill format, slash command unification. HIGH confidence, official docs.
- [MCP Specification 2025-11-25](https://modelcontextprotocol.io/specification/2025-11-25) -- Protocol spec, stdio transport. HIGH confidence, official spec.
- [OpenAI Codex Skills](https://developers.openai.com/codex/skills/) -- Codex skill format, MCP integration. HIGH confidence, official docs.
- [OpenAI Codex MCP](https://developers.openai.com/codex/mcp/) -- Codex MCP configuration in config.toml. HIGH confidence, official docs.
- [Gemini CLI MCP Servers](https://geminicli.com/docs/tools/mcp-server/) -- Gemini MCP configuration in settings.json. HIGH confidence, official docs.
- [Gemini CLI Agent Skills](https://medium.com/google-cloud/beyond-prompt-engineering-using-agent-skills-in-gemini-cli-04d9af3cda21) -- Gemini skill adoption and format. MEDIUM confidence, developer tutorial.
- [OpenCode MCP Servers](https://opencode.ai/docs/mcp-servers/) -- OpenCode MCP configuration. HIGH confidence, official docs.
- [OpenCode Agents](https://opencode.ai/docs/agents/) -- OpenCode agent/skill system. HIGH confidence, official docs.
- GSD v1.22.0 source at `/home/fernando/git/references/get-shit-done/` -- Installer patterns, workflow structure, runtime conversion logic, CJS modules. HIGH confidence, direct code inspection.
- Djinn plugin at `/home/fernando/git/djinn/plugin/` -- Existing plugin structure, SKILL.md, MCP config, hooks, cookbooks. HIGH confidence, direct code inspection.

---
*Stack research for: CLI agent planning/orchestration with MCP integration*
*Researched: 2026-03-02*
