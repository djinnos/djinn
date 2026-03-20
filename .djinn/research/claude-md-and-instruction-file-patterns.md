---
title: CLAUDE.md and Instruction File Patterns
type: research
tags: ["architecture","ai-friendly","claude-md","agents-md","instruction-files"]
---

## Source
- Blog: [Claude Code Best Practices](https://rosmur.github.io/claudecode-best-practices/)
- Blog: [HumanLayer — Writing a Good CLAUDE.md](https://www.humanlayer.dev/blog/writing-a-good-claude-md)
- Blog: [Kirill Markin — Claude Code Rules](https://kirill-markin.com/articles/claude-code-rules-for-ai/)
- Docs: [Anthropic — Using CLAUDE.md Files](https://claude.com/blog/using-claude-md-files)
- HN: [Evaluating AGENTS.md](https://news.ycombinator.com/item?id=47034087)
- Harvested: 2026-03-12

## Research Finding

Human-written AGENTS.md gave ~4% improvement. LLM-generated ones **decreased** performance by 3%.

## File Hierarchy

| File | Scope | VCS |
|------|-------|-----|
| `~/.claude/CLAUDE.md` | All projects (user prefs) | Personal |
| `./CLAUDE.md` | Repository-wide | Committed |
| `./CLAUDE.local.md` | Personal project notes | .gitignore'd |
| `./src/subsystem/CLAUDE.md` | Subsystem-specific | Committed |
| `./agent_docs/` | Deep-dive topics | Committed |

## Content Hierarchy (Highest to Lowest Value)

1. Non-obvious invariants and constraints (tribal knowledge)
2. Exact command invocations with correct flags
3. Module entry points and ownership boundaries
4. "Why we didn't do X" decision rationale
5. Known gotchas from past agent failures

## What NOT to Include

- Code style rules (use a linter instead)
- File-by-file descriptions of the codebase
- Detailed API documentation (link to docs instead)
- Information that changes frequently
- Self-evident practices like "write clean code"
- LLM-generated summaries of discoverable code (no value)
- Code snippets (become stale immediately)

## Progressive Disclosure

Instead of oversized CLAUDE.md, use `agent_docs/` directory:
```
agent_docs/
  auth-patterns.md
  repo-layer-conventions.md
  error-handling-guide.md
```
Reference from CLAUDE.md: "For auth patterns, see agent_docs/auth-patterns.md."

## Size Constraints

- Frontier LLMs reliably follow 150-200 instructions
- Claude Code built-in system prompt uses ~50 slots
- Your budget: ~100-150 instructions max
- Target: under 300 lines root, under 60 ideal
- Instruction-following degrades uniformly as count rises

## Effective Structure (from arXiv manifest study of 253 files)

- Shallow hierarchy: single H1, ~5 H2 subsections, ~9 H3 details
- Most effective content: Build/Run commands (77%), Implementation Details (72%), Architecture (65%), Testing (61%)
- Three heading levels typically suffice

## Relations
- [[Codified Context Three-Tier Architecture]]
- [[Explicit Over Implicit Pattern for AI]]
