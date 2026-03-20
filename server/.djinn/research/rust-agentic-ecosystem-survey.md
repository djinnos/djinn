---
tags:
    - research
    - rust
    - agentic
    - mcp
    - ecosystem
title: Rust Agentic Ecosystem Survey
type: research
---
# Rust Agentic & AI Ecosystem Survey (March 2026)

## Context

If the Djinn server moves to Rust, what replaces Eino (Go agentic framework) and the Go MCP SDK?

---

## Agentic Frameworks

### Rig (rig-rs)

- **Role:** Rust AI/agent framework — closest equivalent to Eino
- **Features:** Agent loops, tool calling, provider abstractions, RAG pipelines
- **Maturity:** Younger than Eino (v0.7, ByteDance-backed) but actively developed
- **Status:** Growing community, not yet battle-tested at Eino's scale

### Comparison with Go Ecosystem

| Capability | Go (Current) | Rust (Available) |
|---|---|---|
| Agentic framework | Eino v0.7 (ByteDance, 9.8k stars) | Rig (younger, growing) |
| MCP SDK | Official v1.4.0 + mcp-go (8.3k stars) | Official Rust MCP SDK |
| HTTP server | net/http + goroutines | Axum + Tokio (production-grade) |
| SQLite | modernc.org/sqlite (pure Go) | rusqlite (mature) or libsql crate (native) |
| Async runtime | Goroutines (built-in) | Tokio (de facto standard) |

### Assessment

The Rust ecosystem is thinner for agentic AI specifically, but the foundation (Axum, Tokio, Serde, rusqlite/libsql) is production-grade. The agentic layer (Rig) is the weakest link — less proven than Eino.

However: Djinn's agent orchestration is custom (Coordinator, Board, task lifecycle). It doesn't deeply depend on a framework like Eino for its core logic. The MCP tool layer and HTTP server are the critical infrastructure pieces, and those are solid in Rust.

---

## MCP in Rust

- **Official Rust MCP SDK** exists (Anthropic-maintained)
- Rust MCP servers are being built in the ecosystem
- The MCP protocol is JSON-RPC over stdio/HTTP — straightforward to implement in any language
- Axum handlers map cleanly to MCP tool endpoints

---

## Server Framework Stack

### Recommended Rust Stack

```
HTTP/MCP:      Axum + Tokio
Database:      libsql crate (native, embedded, DiskANN vectors)
Serialization: Serde (de facto standard)
CLI:           Clap
Testing:       cargo test + tokio::test for async
```

### Axum + Tokio

- Production-grade, widely adopted
- Async by default (tower middleware, extractors)
- Slightly more ceremony than Go's net/http but more type-safe
- Strong ecosystem: tower, hyper, tonic (gRPC)

### libsql Crate (Native)

- libSQL is written in Rust — the Rust crate is first-class
- Embedded mode with local file
- DiskANN vector search available natively
- No FFI, no CGO, no wrapper — direct API
- SQLite compatible (existing data can migrate)

---

## Key Risks

1. **Rig maturity** — may need to build more orchestration logic in-house vs relying on framework
2. **Rust MCP SDK** — less battle-tested than Go's v1.4.0 with 925 dependents
3. **Build times** — Rust full builds are slower than Go; incremental builds are fast
4. **Team expertise** — if humans need to contribute, Rust has a steeper curve (though AI is primary author)

---

## Sources

- [Rig (rig-rs)](https://github.com/rig-rs/rig)
- [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk)
- [Axum](https://github.com/tokio-rs/axum)
- [libSQL](https://github.com/tursodatabase/libsql)
- [Shuttle: AI Coding Tools for Rust](https://www.shuttle.dev/blog/2025/09/09/ai-coding-tools-rust)
- [Red Hat: Why Agentic AI Developers Are Moving to Rust](https://developers.redhat.com/articles/2025/09/15/why-some-agentic-ai-developers-are-moving-code-python-rust)

## Relations
- [[Language Selection — Compiler as AI Code Reviewer]] — language decision context
- [[Embedded Database Survey]] — database layer of the stack
- [[Project Brief]] — project requirements driving stack choices