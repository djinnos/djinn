# Djinn

AI-native development orchestrator. From idea to code using specialized AI personas.

## Install

Download the latest release for your platform from [GitHub Releases](https://github.com/djinnos/djinn/releases).

| Platform | Download |
|----------|----------|
| **macOS (Apple Silicon)** | `Djinn-x.x.x-arm64.dmg` |
| **macOS (Intel)** | `Djinn-x.x.x-x64.dmg` |
| **Linux** | `Djinn-x.x.x-x86_64.AppImage` |
| **Linux (Debian/Ubuntu)** | `Djinn-x.x.x-amd64.deb` |

### Linux (AppImage)

```bash
chmod +x Djinn-*.AppImage
./Djinn-*.AppImage
```

### Linux (deb)

```bash
sudo dpkg -i Djinn-*.deb
```

## Requirements

- An Anthropic API key (or other supported LLM provider)

## What is Djinn?

Djinn provides specialized AI personas (orchestrators) for different stages of software development:

| Persona | Role |
|---------|------|
| **Ana** (Analyst) | Validate ideas, market research, competitive analysis |
| **Archie** (Architect) | System design, ADRs, technical decisions |
| **Ulysses** (UX) | User research, personas, journey mapping |
| **Paul** (PM) | Product planning, epics, stories |
| **Maya** (Marketer) | Go-to-market, growth strategies |
| **Rita** (Recruiter) | Create new agents and skills |

Press **Tab** to switch between personas while preserving context.

## Autonomous Execution

Create tasks via the kanban board, then hit Play. AI agents work on them in parallel in isolated git worktrees with built-in review.

## Documentation

- [Getting Started](https://djinn.dev/docs/getting-started)
- [Orchestrators](https://djinn.dev/docs/orchestrators)
- [Skills](https://djinn.dev/docs/skills)

## License

MIT
