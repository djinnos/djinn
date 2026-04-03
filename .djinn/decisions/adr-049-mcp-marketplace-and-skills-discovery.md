---
title: "ADR-049: MCP Marketplace and Skills Discovery"
type: adr
tags: ["mcp","marketplace","skills","agent-skills","mcp-servers","extensibility"]
---

# ADR-049: MCP Marketplace and Skills Discovery

## Status
Proposed

Date: 2026-04-03

Extends: ADR-038 (Configurable Agent Roles, Domain Specialists, and Auto-Improvement)

## Context

### The Extensibility Gap

Agents need access to external capabilities — web search, documentation lookup, code analysis, custom APIs — to be effective beyond the built-in tool set. The current approach requires manual JSON editing in `.djinn/settings.json` to register MCP servers and manual file creation in `.djinn/skills/` for skills. This is functional but:

1. **Not discoverable** — users don't know what MCP servers exist or which ones would benefit their agents.
2. **Not portable** — users who already configured MCP servers for Claude Code, Cursor, or OpenCode must duplicate that configuration for Djinn.
3. **Not granular** — MCP servers are registered at the project level but there's no ergonomic way to control which agents get which servers. A research agent needs Tavily; a code worker doesn't.
4. **Skills are Djinn-only** — the `.djinn/skills/*.md` format predates the Agent Skills open standard (agentskills.io). Skills written for Claude Code or Copilot aren't discovered.

### Industry Convergence

The ecosystem is converging on two standards:

**MCP servers:** The `mcp.json` file at project root (or tool-specific paths like `.cursor/mcp.json`) is the standard registry format. Djinn should read it rather than maintain a parallel `mcp_servers` key in `.djinn/settings.json`.

**Agent Skills:** The Agent Skills specification (agentskills.io), backed by Anthropic and adopted by Claude Code and GitHub Copilot, defines a directory-based skill format: `<skill-name>/SKILL.md` with YAML frontmatter (`name`, `description`) and markdown body, plus optional `scripts/`, `references/`, and `assets/` subdirectories. The shared discovery path `.claude/skills/` is read by multiple tools.

### Design Constraints

- MCP servers are **per-project** — a project's `mcp.json` defines what's available.
- Agent-to-MCP assignment is **per-agent** — an agent's `mcp_servers` field controls which of the project's available servers it can use.
- Skills are **per-project** but assignment is **per-agent** — an agent's `skills` field controls which skills it receives. Some skills should be assignable globally (all agents in a project).
- Only HTTP-transport MCP servers are supported in agent sessions today. Stdio support is future work.

## Decision

### 1. MCP Server Discovery — Read Standard Config Files

Replace the `mcp_servers` key in `.djinn/settings.json` with multi-source discovery from standard locations. On project load, Djinn reads and merges MCP server definitions from:

1. `mcp.json` (project root — emerging standard)
2. `.cursor/mcp.json`
3. `.opencode/mcp.json`

Merge strategy: first-found wins by server name. If `tavily` is defined in both `mcp.json` and `.cursor/mcp.json`, the `mcp.json` definition takes precedence.

The parsed format follows the standard `mcp.json` schema:

```json
{
  "mcpServers": {
    "tavily": {
      "url": "https://mcp.tavily.com/mcp",
      "headers": { "Authorization": "Bearer ${TAVILY_API_KEY}" }
    },
    "context7": {
      "url": "https://mcp.context7.com/mcp"
    },
    "local-db": {
      "command": "npx",
      "args": ["-y", "db-mcp-server", "--connection", "postgres://..."],
      "env": { "DB_PASSWORD": "${DB_PASSWORD}" }
    }
  }
}
```

Djinn parses this with a simple serde struct — no external crate needed:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    mcp_servers: HashMap<String, McpServerEntry>,
}

#[derive(Deserialize)]
struct McpServerEntry {
    url: Option<String>,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    headers: Option<HashMap<String, String>>,
}
```

Environment variable references (`${VAR_NAME}`) in `url`, `headers`, and `env` values are resolved against the credential store at connection time.

### 2. Agent-to-MCP Assignment

The existing `agent.mcp_servers` field (JSON array of server names) remains the mechanism for controlling which agent gets which MCP servers. The agent can only reference servers that are defined in the project's discovered `mcp.json` sources.

**Chat as an agent type:** The chat interface is effectively an agent for this purpose. It participates in the same MCP discovery and can be assigned MCP servers and skills just like any other agent. This means a user chatting with Djinn can use Tavily for search or Context7 for docs lookups without switching to an autonomous agent session. Chat's MCP/skill assignment is configured via the `"chat"` key in defaults or a dedicated chat settings section.

In `.djinn/settings.json`, a new optional `agent_mcp_defaults` key allows setting project-wide defaults:

```json
{
  "agent_mcp_defaults": {
    "*": ["context7"],
    "chat": ["tavily", "context7"],
    "researcher": ["tavily", "context7"],
    "worker": []
  }
}
```

- `"*"` applies to all agents (including chat) unless overridden by a specific agent name or the agent's own `mcp_servers` field.
- `"chat"` configures the chat interface specifically.
- Agent-level `mcp_servers` (on the Agent model) takes precedence over defaults.
- Empty array `[]` explicitly opts an agent out of defaults.

### 3. MCP Marketplace Catalog

A bundled JSON catalog provides metadata for known MCP servers — descriptions, categories, required credentials, documentation links. This powers a browse-and-install UI; it does not replace `mcp.json` as the configuration source.

Catalog entry schema:

```json
{
  "id": "tavily",
  "name": "Tavily Search",
  "description": "AI-optimized web search. Returns clean results with optional full page content.",
  "category": "search",
  "tags": ["search", "web", "research"],
  "transport": "http",
  "default_config": {
    "url": "https://mcp.tavily.com/mcp",
    "headers": { "Authorization": "Bearer ${TAVILY_API_KEY}" }
  },
  "required_credentials": [
    { "env_var": "TAVILY_API_KEY", "label": "Tavily API Key", "docs_url": "https://tavily.com/#api" }
  ],
  "free_tier": "1,000 searches/month",
  "docs_url": "https://docs.tavily.com",
  "homepage": "https://tavily.com"
}
```

**Install flow:** "Install" takes the catalog entry's `default_config`, writes it into `mcp.json` under the entry's `id` as the server name, and prompts for any `required_credentials`. Credentials are stored via the existing credential system (project-scoped in the DB).

**Catalog updates:** V1 ships the catalog embedded at build time via `include_bytes!`. V2 adds optional remote fetch from a GitHub repository for community-contributed entries.

Initial catalog entries (HTTP-transport, free tier available):
- Tavily (search + extract)
- Context7 (library documentation)
- Brave Search
- Exa (neural search)
- Firecrawl (web crawling)

### 4. Skills Discovery — Read Standard Paths

Extend skill loading to discover skills from multiple standard locations, in priority order:

1. `.claude/skills/<name>/SKILL.md` (Agent Skills standard — shared with Claude Code, Copilot)
2. `.opencode/skills/<name>/SKILL.md`
3. `.djinn/skills/<name>.md` (legacy Djinn format — flat files, backwards compat)
4. `.djinn/skills/<name>/SKILL.md` (Djinn directory format — forwards compat)

Discovery merges all sources. If the same skill name appears in multiple locations, the first-found wins (following the priority order above).

**Format support:**

The Agent Skills standard format (directory with `SKILL.md`):
```
my-skill/
├── SKILL.md            # Required: name, description frontmatter + markdown body
├── scripts/            # Optional: executable helpers
├── references/         # Optional: domain documentation
└── assets/             # Optional: templates, schemas
```

The legacy Djinn format (flat `.md` file) continues to work:
```
my-skill.md             # name, description frontmatter + markdown body
```

Both formats use the same frontmatter contract:
```yaml
---
name: my-skill          # Optional (defaults to filename/dirname)
description: "..."      # Required
---
```

When loading a standard-format skill, `references/` content is appended to the skill body for richer context injection. `scripts/` and `assets/` are made available but not auto-injected.

### 5. Agent-to-Skill Assignment

The existing `agent.skills` field (JSON array of skill names) remains the per-agent assignment mechanism.

A new `global_skills` key in `.djinn/settings.json` defines skills that all agents in the project receive:

```json
{
  "global_skills": ["code-style", "git-workflow"]
}
```

Effective skills for an agent = `global_skills` ∪ `agent.skills`. Agent-level skills are additive — an agent cannot opt out of global skills (they represent project-wide standards).

### 6. Skills Marketplace

Analogous to the MCP marketplace, a bundled catalog of curated skills that users can browse and install. "Install" copies the skill files into `.djinn/skills/<name>/SKILL.md`.

Skills in the catalog are community-contributed prompt templates covering common patterns:
- Language/framework conventions (Rust safety, React patterns)
- Workflow practices (TDD, conventional commits)
- Domain expertise (database migrations, API design)

V1 ships a small curated set. V2 adds a remote catalog.

### 7. REST API and MCP Tools

**REST endpoints** (frontend-facing):

```
GET  /projects/:id/mcp-catalog          # Browse catalog + installed status
POST /projects/:id/mcp-catalog/install  # Install from catalog → writes mcp.json
POST /projects/:id/mcp-catalog/remove   # Remove from mcp.json
GET  /projects/:id/mcp-servers          # List discovered servers (from mcp.json sources)
GET  /projects/:id/skills-catalog       # Browse skills catalog + installed status
POST /projects/:id/skills-catalog/install
GET  /projects/:id/skills              # List discovered skills (from all paths)
```

**MCP tools** (agent-facing):

- `mcp_catalog_list` — browse available MCP servers
- `mcp_catalog_install` — install a server from catalog
- `mcp_servers_list` — list project's discovered MCP servers
- `skills_catalog_list` — browse available skills
- `skills_catalog_install` — install a skill from catalog
- `skills_list` — list project's discovered skills

### 8. Frontend

A new "Extensions" tab in project settings, with two sub-sections:

**MCP Servers:**
- Grid of catalog entries with install/remove actions
- Credential prompts for servers that need API keys
- Per-agent assignment UI in the agent edit form (already has `mcp_servers` field)
- Status indicators: installed, needs credentials, connected, HTTP-only vs stdio (coming soon)

**Skills:**
- Grid of catalog entries with install actions
- Per-agent assignment in agent edit form (already has `skills` field)
- Global skills toggle (adds/removes from `global_skills` in settings)

### 9. Migration

- Existing `mcp_servers` entries in `.djinn/settings.json` are migrated to `mcp.json` at project root on first load. The key is removed from settings after migration.
- Existing `.djinn/skills/*.md` flat files continue to work without changes.
- No database migration needed — `agent.mcp_servers` and `agent.skills` fields are unchanged.

## Implementation Phases

### Phase 1: MCP Discovery + Skills Discovery
- Parse `mcp.json`, `.cursor/mcp.json`, `.opencode/mcp.json`
- Add `headers` support to MCP client transport
- Credential/env-var resolution in server configs
- Extend skill loader to read `.claude/skills/`, `.opencode/skills/`, directory format
- Migrate existing `mcp_servers` from settings to `mcp.json`
- Drop `mcp_servers` from `DjinnSettings`

### Phase 2: Catalogs + REST API
- Embed MCP server catalog JSON
- Embed skills catalog JSON
- `McpCatalogService` and `SkillsCatalogService`
- REST endpoints for browse/install/remove
- MCP tools for agent-facing catalog access

### Phase 3: Frontend
- Extensions tab in project settings
- MCP server grid with install/credential flow
- Skills grid with install flow
- Agent edit form: MCP server picker, skill picker
- Global skills toggle

### Phase 4: Agent Defaults + Polish
- `agent_mcp_defaults` in settings
- `global_skills` in settings
- Connection health checks for installed MCP servers
- Catalog refresh from remote source (v2)

## Consequences

### Positive
- Zero-config for users who already have `mcp.json` — their servers just work in Djinn
- Agents become meaningfully extensible without code changes
- Skills written for Claude Code or Copilot are automatically discovered
- Marketplace lowers the barrier to adding capabilities like web search or doc lookup
- Per-agent assignment keeps agents focused — a code worker doesn't get noisy search tools

### Negative
- Multiple discovery paths add complexity to config resolution and debugging
- Bundled catalog requires Djinn releases to add new entries (until remote fetch in v2)
- HTTP-only transport limits which community MCP servers are usable today

### Risks
- `mcp.json` format is not yet formally standardized — may diverge across tools. Mitigation: the format is simple and Djinn's parser is tolerant.
- Credential resolution via `${ENV_VAR}` in JSON requires careful handling to avoid leaking secrets in logs or error messages.
