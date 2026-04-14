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

## Using Djinn with Your Own Tools

Djinn runs a background daemon (`djinn-server`) that exposes task management and persistent memory as MCP tools. The daemon starts automatically and stays running between sessions — Djinn Desktop does not need to be open.

Any MCP-compatible tool can connect via the `djinn-server --mcp-connect` stdio bridge, which auto-discovers and auto-starts the daemon.

> **Note:** Djinn Desktop must have been launched at least once to install `djinn-server` on your PATH.

### Claude Code

Djinn Desktop automatically installs a [Claude Code plugin](https://code.claude.com/docs/en/plugins) when it detects Claude Code on your system. No manual setup required — just have both installed.

The plugin gives you:
- The `djinn` skill — auto-activates when you work with tasks, memory, or execution
- Djinn MCP tools (tasks, memory, settings, projects, execution) via `djinn-server`
- Session hooks that auto-start the Djinn daemon

The skill is workflow-agnostic — it teaches Claude Code how to use Djinn's task board, memory system, and execution engine without prescribing any specific methodology. Everything is namespaced under `djinn:` to avoid conflicts with your own customizations.

### Cursor

Add to `.cursor/mcp.json` in your project:

```json
{
  "mcpServers": {
    "djinn": {
      "command": "djinn-server",
      "args": ["--mcp-connect"]
    }
  }
}
```

### Windsurf

Add to your Windsurf MCP config:

```json
{
  "mcpServers": {
    "djinn": {
      "command": "djinn-server",
      "args": ["--mcp-connect"]
    }
  }
}
```

### OpenCode

If you have [OpenCode](https://opencode.ai) installed separately, add to your `opencode.json`:

```json
{
  "mcp": {
    "djinn": {
      "type": "local",
      "command": ["djinn-server", "--mcp-connect"],
      "enabled": true
    }
  }
}
```

To also get Djinn's skills, use the bundled config:

```bash
OPENCODE_CONFIG=~/.djinn/opencode.json \
OPENCODE_CONFIG_DIR=~/.djinn/.opencode \
opencode
```

### Any MCP Client

Any tool that supports [MCP](https://modelcontextprotocol.io) can connect via stdio:

```bash
djinn-server --mcp-connect
```

Or via HTTP if your client doesn't support stdio:

```bash
# Find the daemon's HTTP endpoint
cat ~/.djinn/server.json
# { "port": 4440, "url": "http://localhost:4440/mcp", ... }
```

### Linux memory mount (ADR-057 wave 3 guidance)

Djinn ships a Linux-only FUSE mount for repository-backed memory behind an explicit build and settings gate. It is **disabled by default** and is intended for filesystem-first note workflows once agents no longer rely on broad MCP CRUD affordances.

Enable it by building `djinn-server` with the cargo feature and then setting:

```json
{
  "memory_mount_enabled": true,
  "memory_mount_path": "/absolute/empty/mountpoint"
}
```

Example startup flow:

```bash
cd server
cargo run --features memory-mount --bin djinn-server
```

Current supported constraints and guardrails:
- Linux only
- requires FUSE host support (`/dev/fuse`, kernel/userspace FUSE tooling, and permission to mount)
- `memory_mount_path` must already exist, be absolute, and be empty at startup
- only a single registered project is supported by this initial mount slice
- the mounted tree exposes the **current session view**, not explicit branch directories
- when Djinn can resolve one active task with a non-canonical worktree for the mounted project, `.djinn/memory/` reflects that task/worktree view
- if no active task/session/worktree can be resolved, or the active session is still on the canonical project root, the mount **falls back to the canonical `main` view**
- agents/operators should therefore treat `.djinn/memory/` as a branch-aware live view of the current session, not as proof that they are writing to an isolated branch
- no additional branch directory UX (`@main`, `@task_*`, symlink switching, etc.) is supported in this slice
- macOS fallback transport and broader multi-project operational hardening remain deferred to later ADR-057 waves

If the configuration is invalid, server startup fails early with a clear error instead of silently serving without the mount.

Filesystem-first usage guidance:
- prefer normal file reads/writes/edits under `.djinn/memory/` when the mount is enabled
- keep MCP memory tools for analytical flows such as context assembly, search/health, and compatibility-only cases
- if you need a guaranteed canonical view, use the checked-in `.djinn/` tree or analytical MCP reads rather than assuming the mount stayed on `main`
- if you are unsure what view the mount is serving, inspect the server runtime status surface (`GET /health` → `memory_mount`) before making broad note edits; it reports whether the mount is merely configured, actively mounted, or degraded
- treat a mounted `.djinn/memory/` tree as the live session-selected view for the current task/worktree, not as an isolated branch checkout with its own branch-named directories

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
