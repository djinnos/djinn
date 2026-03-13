---
title: Agent Parallelism and Structural Isolation
type: research
tags: ["architecture","ai-friendly","parallelism","worktrees","multi-agent"]
---

## Source
- HN: [Claude Code Best Practices](https://news.ycombinator.com/item?id=43735550)
- HN: [Agentic Coding 101](https://news.ycombinator.com/item?id=46877429)
- Blog: [Anthropic — Effective Context Engineering](https://www.anthropic.com/engineering/effective-context-engineering-for-ai-agents)
- Harvested: 2026-03-12

## Structural Requirements for Parallel Agents

Multiple simultaneous agents require:
- **Git worktrees** — separate working trees per agent, no blocking
- **Feature isolation** — vertical slices minimize merge conflicts
- **Clear ownership boundaries** — CLAUDE.md defines which agent owns which module
- **Agents coordinate via documentation, not direct communication**

## Multi-Agent Patterns

### Orchestrator + Explorer + Coder
Three-role architecture with intentional access constraints:
- **Orchestrator**: Strategic planning, no direct code access (forces systematic thinking)
- **Explorer**: Read-only investigation, produces distilled findings (not raw file dumps)
- **Coder**: Full read-write access, receives pre-distilled context

Sub-agents report condensed findings (1,000-2,000 tokens) rather than raw content.

### Writer/Reviewer Parallel Sessions
- Session A implements a feature
- Session B reviews with fresh context (not biased toward code it just wrote)
- Feedback loops back to Session A
- Fresh context produces better reviews than accumulated context

### Subagent Delegation for Investigation
Use subagents for codebase exploration so investigation doesn't consume main session context.

## Workflow Architecture

1. Each agent gets a worktree + focused task with explicit file list
2. Agents commit frequently (safe rollback points)
3. Human reviews between merge steps — not autonomous end-to-end
4. CLAUDE.md defines the handoff protocol

## Context Lifecycle

- Start new conversations when shifting tasks or agent confusion occurs
- Long conversations accumulate noise — effectiveness decreases after many turns
- Compaction: summarize history before context limit; preserve decisions, discard noise
- Structured note-taking (NOTES.md, plan.md) for progress tracking across sessions

## Relations
- [[Vertical Slice Architecture for AI]]
- [[Codified Context Three-Tier Architecture]]
