<p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/icon.png?raw=true" width="128" height="128" alt="Djinn" />
</p>

<h1 align="center">Djinn</h1>

<p align="center">
  <strong>Autonomous AI agents that build software while you sleep.</strong>
  <br />
  Local-first. Any LLM. Parallel execution with built-in review.
</p>

<p align="center">
  <a href="https://github.com/djinnos/djinn/releases"><strong>Download</strong></a> ·
  <a href="https://djinn.dev"><strong>Website</strong></a> ·
  <a href="https://djinn.dev/docs"><strong>Docs</strong></a> ·
  <a href="https://discord.gg/djinn"><strong>Discord</strong></a>
</p>

<br />

> **Linear organizes work for humans. Djinn organizes work for AIs.**

Djinn is an AI-native project management and autonomous development orchestrator. Create hundreds of tasks — features, tech debt, bugs — and have AI agents work on them in parallel, 24/7, on your machine, with any LLM provider.

It's not just another coding agent. It's the first **task management system built for AI workers**: epics, dependencies, review pipelines, persistent memory, and architectural quality gates — all designed for autonomous agents, not humans with keyboards.

<br />

<!-- TODO: Replace with actual screenshot -->
<!-- <p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/screenshot.png?raw=true" width="800" alt="Djinn Desktop — Kanban board with autonomous execution" />
</p> -->

## How It Works

```
  Create tasks ──→ Hit Play ──→ Agents work in parallel ──→ Review ──→ Merge
       │                │                │                      │
   Kanban board    Coordinator     Isolated git worktrees    SM + Architect
   or CLI          spawns agents   one per task              review pipeline
```

1. **Create tasks** — Features, bugs, tech debt. Organize as epics with dependencies and blockers. Use the kanban board or let AI decompose work for you.
2. **Hit Play** — The coordinator spawns AI agents in isolated git worktrees, respecting dependency order.
3. **Agents work in parallel** — Multiple dev agents execute simultaneously, each sandboxed. Circuit breakers catch stuck agents.
4. **Built-in review** — An SM agent reviews each task against acceptance criteria. An Architect agent reviews every N tasks for architectural drift.
5. **Merge when ready** — Verified work merges to main automatically.

## Why Djinn

|   | Djinn | Codex | Devin | Cursor |
|---|-------|-------|-------|--------|
| Parallel agents | ✅ Unlimited | ❌ | ❌ | ❌ |
| Runs locally | ✅ Your machine | ❌ Cloud | ❌ Cloud | ✅ |
| Any LLM | ✅ BYO keys | ❌ OpenAI only | ❌ Proprietary | ⚠️ Limited |
| Task management | ✅ Built-in | ❌ | ⚠️ Basic | ❌ |
| Review pipeline | ✅ SM + Architect | ❌ | ⚠️ | ❌ |
| Persistent memory | ✅ Knowledge base | ❌ | ❌ | ❌ |
| Price | **Free (beta)** | $200/mo | $500+/mo | $20/mo |

**Your code never leaves your machine.** Djinn runs entirely on your infrastructure. Agents are sandboxed to only the projects you specify.

## Install

Download the latest release for your platform:

<table>
  <tr>
    <td>🍎 <strong>macOS</strong></td>
    <td>
      <a href="https://github.com/djinnos/djinn/releases/latest">Apple Silicon (.dmg)</a> ·
      <a href="https://github.com/djinnos/djinn/releases/latest">Intel (.dmg)</a>
    </td>
  </tr>
  <tr>
    <td>🐧 <strong>Linux</strong></td>
    <td>
      <a href="https://github.com/djinnos/djinn/releases/latest">AppImage</a> ·
      <a href="https://github.com/djinnos/djinn/releases/latest">.deb</a>
    </td>
  </tr>
  <tr>
    <td>🪟 <strong>Windows</strong></td>
    <td>
      <a href="https://github.com/djinnos/djinn/releases/latest">Installer (.exe)</a>
    </td>
  </tr>
</table>

> **Requirement:** An API key from any supported LLM provider (Anthropic, OpenAI, Google, or local models).

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

### 🎯 AI-Native Task Management

Epics, stories, blockers, dependencies, and priority ordering — designed for AI agents as workers, humans as directors. Manage everything from the kanban board or CLI.

### ⚡ Parallel Autonomous Execution

Spawn multiple AI agents that work simultaneously. Each gets its own isolated git worktree. No conflicts. No stepping on toes. Run overnight and wake up to completed work.

### 🔍 Built-in Review Pipeline

Every task is reviewed by an SM agent against acceptance criteria. Every N tasks, an Architect agent reviews the batch for architectural drift and can spawn corrective tasks.

### 🧠 Persistent Memory

A human-readable knowledge base (markdown, Obsidian-compatible) that compounds over time. Architectural decisions, patterns, and project context persist across sessions. You can read, edit, and version control what the AI knows.

### 🔑 Any LLM Provider

Bring your own API keys. Claude, GPT, Gemini, local models — switch providers anytime. No vendor lock-in.

### 🏠 Local-First & Secure

Everything runs on your machine. Code never touches external servers. Agents are sandboxed to only the projects you specify with limited permissions.

## AI Personas

Djinn includes specialized AI personas (orchestrators) for every stage of development:

| Persona | Focus |
|---------|-------|
| **Analyst** | Market research, competitive analysis, idea validation |
| **Architect** | System design, ADRs, technical decisions |
| **UX Designer** | User research, personas, journey mapping |
| **Product Manager** | Epics, stories, roadmap planning |
| **Growth Marketer** | Go-to-market, content, growth strategy |
| **Recruiter** | Create new agents and skills for your project |

Press **Tab** to switch between personas. Context is preserved across switches.

## Tech Stack

- **Desktop:** Electron + React + Tailwind
- **Server:** Go (event-driven coordinator)
- **Agent Engine:** [OpenCode](https://opencode.ai) (open-source)
- **Task DB:** Per-project SQLite, loaded from all registered repos
- **Memory:** Markdown knowledge base with semantic linking

## Documentation

- [Getting Started](https://djinn.dev/docs/getting-started)
- [Orchestrators & Personas](https://djinn.dev/docs/orchestrators)
- [Skills & Thinking Techniques](https://djinn.dev/docs/skills)
- [Autonomous Execution](https://djinn.dev/docs/auto-dev)
- [Memory System](https://djinn.dev/docs/memory)

## Community

- [Discord](https://discord.gg/djinn) — Chat, questions, show & tell
- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

[MIT](LICENSE)
