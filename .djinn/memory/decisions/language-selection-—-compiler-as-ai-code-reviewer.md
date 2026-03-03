---
tags:
    - adr
    - language
    - rust
    - compiler-safety
title: Language Selection — Compiler as AI Code Reviewer
type: adr
---
# ADR-001: Language Selection — Compiler as AI Code Reviewer

**Status:** Accepted
**Date:** 2026-03-02
**Context:** Djinn server rewrite — choosing implementation language given AI is the primary code author

---

## Context

Djinn's server is currently written in Go. AI (Claude, via Claude Code and autonomous agents) writes the majority of the code. The question: which language best prevents AI from writing bad code?

This is not about developer ergonomics or hiring. The primary code author is an LLM. The language's type system and compiler act as the safety net.

---

## Decision Drivers

1. **AI writes most of the code** — the compiler is the primary reviewer
2. **Concurrency is core** — MCP server, SSE, background tasks
3. **libSQL is written in Rust** — first-class Rust SDK vs second-class Go FFI wrapper
4. **Long-term correctness** — silent bugs in production are more expensive than compile-time friction
5. **Desktop reads the same DB file** — no sync plumbing if both processes can access SQLite/libSQL

---

## Options Considered

### Option A: Stay with Go

**Pros:**
- Zero rewrite cost
- Eino (v0.7, ByteDance-backed) is the most mature Go agentic framework
- Official MCP Go SDK v1.4.0, stable
- Goroutines are simple concurrency model
- AI generates Go faster per cycle

**Cons:**
- Go's compiler does NOT catch concurrency bugs (goroutine leaks, data races, closure capture)
- Uber found 2,000+ data races in 46M lines of Go that passed all tests and code review
- AI generates 2x more concurrency bugs than humans (CodeRabbit, n=470 PRs)
- go-libsql is v0.0.0, pre-release, no Windows — libSQL is second-class in Go
- `nil` dereferences are a runtime panic, not a compile error

### Option B: Rewrite in Rust

**Pros:**
- ~40% of AI compile errors are borrow checker violations that would be silent production bugs elsewhere
- When Rust compiles: memory corruption, data races, use-after-free are **mathematically absent**
- libSQL is native Rust — first-class SDK, embedded DiskANN vector search
- Compiler errors are machine-readable (error codes, suggestions) — ideal for AI iteration loops
- AI doesn't get frustrated by the borrow checker; it iterates 30 times as readily as 3
- Multi-SWE-bench: Rust (15.9% pass@1) outperforms Go (7.5%) on real-world issue resolution
- Google reduced Android memory safety vulns from 76% to 24% via Rust adoption

**Cons:**
- Full rewrite cost (though AI bears most of this)
- Rig (Rust agentic framework) is younger than Eino
- Rust MCP SDK is less battle-tested than Go's
- Smaller ecosystem for AI/agent tooling vs Go
- Steeper learning curve for human contributors (if any)

### Option C: TypeScript (Electron-native)

**Rejected.** AI uses `any` at 9x the rate of human developers (study: 38,979 PRs). TypeScript safety is systematically undermined by AI code generation. Type assertions used to silence errors rather than fix them. The type system is opt-in in practice.

---

## Empirical Evidence

### AI Code Bug Rates (CodeRabbit, Dec 2025, n=470 PRs)

| Metric | AI Code | Human Code | Ratio |
|---|---|---|---|
| Issues per PR | 10.83 | 6.45 | 1.68x |
| Logic & correctness | — | — | 1.75x more in AI |
| Security issues | — | — | 2.74x more in AI |
| Concurrency bugs | — | — | ~2x more in AI |

### TypeScript: AI Undermines the Type System (Feb 2026, n=38,979 PRs)

- AI introduces `any` at **9x the rate** of human developers
- AI adds 2-2.5x more `any` than it removes per PR
- AI TypeScript PRs accepted at 45.8% despite worse type safety — reviewers don't catch it

### Multi-SWE-bench: Real-World Issue Resolution (April 2025)

| Language | Pass@1 |
|---|---|
| Rust | 15.9% |
| Go | 7.5% |
| JavaScript | 5.1% |
| TypeScript | 2.2% |

### Go Concurrency Blind Spot (Uber, PLDI 2022)

- 2,000+ data races found in 46M LOC Go monorepo
- All had passed code review and testing
- Go compiler catches none of them
- Race detector is runtime-only, not enabled in production

### Rust Compiler as Guardrail

- ~40% of AI compile errors are borrow checker violations
- Average 3 compiler cycles per task before resolution
- Structured error codes (E0106, E0502) with suggested fixes — machine-readable
- When `cargo build` succeeds: memory safety and data race freedom are proven

### Android Memory Safety (Google, 2024-2025)

- Memory safety vulnerabilities: 76% (2019) -> 24% (2024) -> <20% (2025)
- Directly correlated with Rust adoption
- Rust change rollback rate is less than half that of C++

---

## Decision

**Accepted: Option B — Rust**

The rewrite cost is real but bounded. AI bears most of it. The long-term payoff is:
- Every concurrent bug caught at compile time instead of production
- libSQL as a first-class native dependency
- DiskANN vector search without FFI wrappers
- A codebase where `cargo build` succeeding is a meaningful correctness signal

---

## Consequences

**Positive:**
- Compiler catches memory and concurrency bugs that would silently ship in Go
- libSQL/Turso is native — no FFI wrapper, no v0.0.0 dependency
- Vector search (DiskANN) available in embedded mode
- AI iteration loop (write -> compile -> fix -> compile) is natural and effective

**Negative:**
- Full server rewrite required
- Rust agentic framework ecosystem is less mature than Go's
- Human contributors face steeper learning curve
- Build times longer than Go (though incremental builds help)

---

## Sources

- [Mining Type Constructs in AI-Generated Code](https://arxiv.org/html/2602.17955)
- [Multi-SWE-bench](https://arxiv.org/html/2504.02605v1)
- [CodeRabbit AI vs Human Code Report](https://www.coderabbit.ai/blog/state-of-ai-vs-human-code-generation-report)
- [Uber Data Race Patterns in Go](https://www.uber.com/en-US/blog/data-race-patterns-in-go/)
- [Google: Eliminating Memory Safety Vulnerabilities](https://security.googleblog.com/2024/09/eliminating-memory-safety-vulnerabilities-Android.html)
- [RustEvo2 Benchmark](https://arxiv.org/html/2503.16922)
- [RustForger / Rust-SWE-bench](https://arxiv.org/html/2602.22764v1)
- [Prediction: AI Will Make Formal Verification Mainstream](https://martin.kleppmann.com/2025/12/08/ai-formal-verification.html)
- [Security Weaknesses of Copilot-Generated Code](https://arxiv.org/html/2310.02059v3)

## Relations
- [[Project Brief]] — project context driving this decision
- [[Embedded Database Survey]] — database choice depends on language decision
- [[Rust Agentic Ecosystem Survey]] — ecosystem viability for Rust