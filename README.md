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

<!-- TODO: Replace with actual screenshot -->
<!-- <p align="center">
  <img src="https://github.com/djinnos/djinn/blob/main/.github/assets/screenshot.png?raw=true" width="800" alt="Djinn Desktop — Kanban board with parallel AI agents across multiple projects" />
</p> -->

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

## Community

- [GitHub Issues](https://github.com/djinnos/djinn/issues) — Bug reports and feature requests
- [GitHub Discussions](https://github.com/djinnos/djinn/discussions) — Ideas and general conversation

## License

Proprietary. © 2026 Djinn AI, Inc. Free to use during beta.
