# Djinn

Agentic coding framework that goes from idea to code using orchestrator personas.

## Install

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/djinnos/djinn/main/install.sh | bash
```

**npm** (Node.js 16+):
```bash
npm install -g djinn-cli
```

**npx** (run without installing):
```bash
npx djinn-cli
```

## Requirements

- [OpenCode](https://opencode.ai) - Djinn uses OpenCode as the underlying AI coding assistant
- An Anthropic API key (or other supported provider)

## Quick Start

1. Install djinn (see above)
2. Navigate to your project directory
3. Run `djinn`

```bash
cd my-project
djinn
```

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

For autonomous task execution from your backlog:

```bash
djinn auto-dev                    # Run until no tasks
djinn auto-dev --dry-run          # Preview without executing
```

## Documentation

- [Getting Started](https://djinn.dev/docs/getting-started)
- [Orchestrators](https://djinn.dev/docs/orchestrators)
- [Skills](https://djinn.dev/docs/skills)

## License

MIT
