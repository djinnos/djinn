---
title: Tests as AI Safety Net
type: research
tags: ["architecture","ai-friendly","testing","behavioral-contracts","verification"]
---

## Source
- HN: [LLM Coding in Established Codebases](https://news.ycombinator.com/item?id=46292682)
- HN: [Experience with Agentic Coding](https://news.ycombinator.com/item?id=46125341)
- Blog: [Addy Osmani — The 70% Problem](https://addyo.substack.com/p/the-70-problem-hard-truths-about)
- Harvested: 2026-03-12

## The 4,000-5,000 LOC Breaking Point

Pure vibe coding without tests invariably produces slop by this size. Architecture degrades because each agent interaction has no ground truth to correct against.

## Behavioral Testing for AI

Test **observable outputs through public interfaces**, not implementation details. This lets AI refactor internals freely as long as behavioral tests pass.

- Tests serve as executable specifications AI can reason against
- AI without verification becomes dependent on human review for every change
- Tests lock behavioral contracts so AI cannot accidentally change semantics

**Implementation-detail testing is harmful for AI**: Tests that reach into private state prevent refactoring — AI must either break tests or avoid refactoring, neither correct.

## Test Architecture Rules

- Tests must be **tightly scoped** — monolithic test files that bundle unrelated tests force agents to read irrelevant code
- Tests are ideal LLM generation targets: self-contained, lower stakes, verifiable
- **Test-first** on new features: write the test spec, have the agent implement to green
- **Fast feedback loops** are disproportionately important — agents iterate; slow suites multiply time

## The 70% Ceiling

AI gets prototype quality to ~70% quickly, then stalls. The remaining 30% requires security, performance tuning, edge cases, and polish.

Pattern that works for full 100%:
1. **AI First Draft**: Generate implementation
2. **Human Refactor**: Modularize, add error handling, write tests, document decisions
3. **Fresh Sessions**: New AI sessions per distinct task
4. **Trust But Verify**: Review critical paths manually, automate edge-case testing

## File Size Limits for AI

Without explicit limits, AI generates 1000+ line files with deep nesting. Recommended:
- Max file size: **400 lines**
- Max cyclomatic complexity per function: **12**
- Max indentation level: **3**

## Relations
- [[Deep Modules Pattern for AI Codebases]]
- [[Pit of Success Pattern for AI]]
- [[Vertical Slice Architecture for AI]]
