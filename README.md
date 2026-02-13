<p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/icon.png?raw=true" width="128" height="128" alt="Djinn" />
</p>

<h1 align="center">Djinn</h1>

<p align="center">
  <strong>Manage AI agents, not terminals.</strong>
  <br />
  Local-first. Multi-project. Mix and match any LLM. You stay in control.
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/releases"><strong>Download</strong></a> ·
  <a href="https://djinnai.io"><strong>Website</strong></a>
</p>

<br />

Djinn is an AI development orchestrator. Organize work across multiple projects as epics and tasks, run AI agents in parallel on your machine, and review every decision before it merges.

Instead of juggling terminal windows and manually switching between models and repos, you direct work from a kanban board. Djinn handles the execution — you review the results.

<br />

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/kanban.jpg?raw=true" width="800" alt="Djinn Desktop — Kanban board with parallel AI agents across multiple projects" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/epics.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/epics.jpg?raw=true" width="800" alt="Djinn Roadmap — Epic dependency graph with tasks and blockers" />
  </a>
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/blob/main/.github/assets/memory.jpg?raw=true">
    <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/memory.jpg?raw=true" width="800" alt="Djinn Memory Graph — Knowledge base visualization with connected notes" />
  </a>
</p>

## How It Works

```
  Create tasks ──→ Hit Play ──→ Agents work in parallel ──→ Review ──→ Merge
       │                │                │                      │
   Kanban board    Coordinator     Isolated git worktrees    You review the
   or CLI          spawns agents   one per task              finished work
```

1. **Create tasks** — Features, bugs, tech debt. Organize as epics with dependencies and blockers across any number of projects.
2. **Hit Play** — The coordinator spawns AI agents in isolated git worktrees, respecting dependency order.
3. **Agents work in parallel** — Multiple dev agents execute simultaneously, each sandboxed in its own worktree.
4. **You review** — AI reviewers check each task against your acceptance criteria. You review the finished work and decide when to merge.

## Install

Download the latest release for your platform:

<table>
  <tr>
    <td>🍎 <strong>macOS</strong></td>
    <td><a href="https://djinnai.io/api/download?platform=mac-arm64">Apple Silicon (.dmg)</a></td>
  </tr>
  <tr>
    <td>🐧 <strong>Linux</strong></td>
    <td><a href="https://djinnai.io/api/download?platform=linux-appimage">AppImage</a> · <a href="https://djinnai.io/api/download?platform=linux-deb">.deb</a></td>
  </tr>
  <tr>
    <td>🪟 <strong>Windows</strong></td>
    <td><a href="https://djinnai.io/api/download?platform=windows">Installer (.exe)</a></td>
  </tr>
</table>

> Works with any LLM provider supported by [OpenCode](https://opencode.ai) — use your existing subscription plans or API keys.

<details>
<summary>Linux install instructions</summary>

**AppImage:**
```bash
chmod +x Djinn-*.AppImage
./Djinn-*.AppImage
```

**Debian/Ubuntu:**
```bash
sudo dpkg -i Djinn-*.deb
```
</details>

## Features

### ⚡ Parallel Execution

Run multiple AI agents in parallel, each in its own isolated git worktree. Manage tasks on a kanban board instead of switching between terminal windows.

### 📁 Multi-Project

Microservices, monorepos, multiple repositories — Djinn manages them all in parallel. Each project has its own task database and knowledge base. One app to direct everything.

### 🔀 Mix & Match Models

Works with any provider supported by OpenCode — Claude, GPT, Gemini, local models, and more. Use them all at the same time: one model for coding, another for reviews, another for research. Configure which models handle which tasks and at what priority. Use your existing plans or API keys.

### 🧠 Persistent Memory

Decisions, patterns, and architectural rules live in a human-readable knowledge base — markdown files you can read, edit, and version control. You decide what context agents get.

### 🔍 Built-in Review

AI reviewers check each task against your acceptance criteria. You review the finished work and decide when to merge. Nothing ships without your approval.

### 🏠 Local-First

Everything runs on your machine. Your code never touches external servers. Agents are sandboxed to only the projects you specify.

## AI Personas

Djinn includes specialized AI personas (orchestrators) for different stages of development:

| Persona | Focus |
|---------|-------|
| **Analyst** | Market research, competitive analysis, idea validation |
| **Architect** | System design, ADRs, technical decisions |
| **UX Designer** | User research, personas, journey mapping |
| **Product Manager** | Epics, stories, roadmap planning |
| **Growth Marketer** | Go-to-market, content, growth strategy |
| **Recruiter** | Create new agents and skills for your project |

Press **Tab** to switch between personas. Context is preserved across switches.

## Using Djinn with Your Own Tools

Djinn Desktop runs an MCP server that exposes task management and persistent memory as tools. While the embedded OpenCode instance connects automatically, you can also use these tools from **your own OpenCode installation**, **Claude Code**, **Cursor**, or any MCP-compatible client.

> **Important:** Djinn Desktop must be running for MCP tools to be available. The MCP server starts with the desktop app — external tools connect to it, they don't start their own server. Launch Djinn Desktop first, then start your external tool.

The MCP endpoint is:

```
http://localhost:4440/mcp
```

<details>
<summary>Port configuration</summary>

Djinn defaults to port `4440`. If that port is busy, it automatically picks an available port and saves it for next time. You can change the port in Djinn Desktop settings.

If you need to check the current port programmatically:
```bash
cat ~/.djinn/server.json
# { "port": 4440, "url": "http://localhost:4440/mcp", ... }
```
</details>

### OpenCode (Native Install)

If you have [OpenCode](https://opencode.ai) installed separately, you can run it with Djinn's full config — all personas, skills, and MCP tools:

```bash
OPENCODE_CONFIG=~/.djinn/opencode.json \
OPENCODE_CONFIG_DIR=~/.djinn/.opencode \
opencode
```

This gives you:
- All Djinn personas (Analyst, Architect, PM, UX, Marketer, Recruiter)
- All skills (TDD, debugging, Go best practices, React best practices, etc.)
- Djinn MCP tools (tasks, memory, settings, projects)
- Third-party MCPs (grep, context7)

> **Tip:** Create a shell alias for convenience:
> ```bash
> alias djinn-opencode='OPENCODE_CONFIG=~/.djinn/opencode.json OPENCODE_CONFIG_DIR=~/.djinn/.opencode opencode'
> ```

You can also load just the personas and skills without the Djinn MCP server:

```bash
OPENCODE_CONFIG_DIR=~/.djinn/.opencode opencode
```

Task and memory tools won't be available, but you'll get all the personas and skills. Useful when working offline or without Djinn Desktop.

### Claude Code

Add to your project's `.mcp.json` (or `~/.claude.json` for global):

```json
{
  "mcpServers": {
    "djinn": {
      "type": "url",
      "url": "http://localhost:4440/mcp"
    }
  }
}
```

### Cursor

Add to `.cursor/mcp.json` in your project:

```json
{
  "mcpServers": {
    "djinn": {
      "url": "http://localhost:4440/mcp"
    }
  }
}
```

### Any MCP Client

Any tool that supports the [MCP protocol](https://modelcontextprotocol.io) can connect to Djinn's HTTP+SSE endpoint at `http://localhost:4440/mcp`.

### What's Available Over MCP

| Tool Group | Examples | Description |
|-----------|----------|-------------|
| **Tasks** | `task_create`, `task_list`, `task_show`, `task_transition` | Full task lifecycle — create, update, transition, comment |
| **Memory** | `memory_search`, `memory_read`, `memory_write` | Knowledge base — ADRs, patterns, research notes |
| **Projects** | `projects_list`, `projects_add` | Multi-project management |
| **Settings** | `settings_get`, `settings_save` | Configuration management |
| **Execution** | `execution_start`, `execution_pause` | Control the task executor |

## Community

- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

Proprietary. © 2026 Djinn AI, Inc. Free to use during beta.
