---
title: ADR-019: MCP as Single API and Typed Tool Schemas
type: adr
tags: ["adr","mcp","api","types"]
---

# ADR-019: MCP as Single API and Typed Tool Schemas

## Status
Accepted

Date: 2026-03-05

## Context

The server exposes MCP at `/mcp` plus infrastructure endpoints (`/health`, `/events`, `/db-info`). Desktop code was attempting to call REST routes such as `/providers/catalog` and `/credentials/*` that do not exist.

Maintaining a second REST surface for the same domain operations would duplicate contracts, add drift risk, and create extra maintenance overhead for tool evolution.

At the same time, strong type safety is required across desktop and server. MCP already carries JSON Schema for tool inputs and outputs, but several tool handlers currently build responses via ad-hoc `serde_json::json!` values, which reduces output schema precision.

## Decision

1. **MCP is the server's single domain API surface.**
   - No parallel REST routes for provider/credential/task/epic domain actions.
   - Domain changes are made through MCP tool contracts only.

2. **Tool responses are upgraded to typed structs where practical.**
   - Replace ad-hoc JSON response builders with concrete Rust response types.
   - Derive `Serialize` + `JsonSchema` on response structs to emit precise MCP output schemas.
   - Keep dynamic JSON only where values are truly unbounded.

3. **Schema-first client generation is the default integration path.**
   - Desktop codegen consumes MCP `tools/list` schemas for generated TS types.
   - Input schemas must remain strict and stable; output schemas should be tightened as typed responses are introduced.

## Consequences

**Positive:**
- One canonical API contract (MCP) across server and desktop
- Less API drift and lower maintenance cost than dual MCP + REST
- Better generated types as output schemas become concrete
- Faster tool evolution without keeping duplicate handlers in sync

**Negative:**
- Existing ad-hoc response paths require refactors to typed structs
- Output type quality may be mixed until conversion is complete
- MCP JSON-RPC envelope handling remains a client concern

## Relations
- [[decisions/adr-003-split-epic-and-task-mcp-tools-with-input-validation|ADR-003: Split Epic and Task MCP Tools with Input Validation]]
- [[ADR-018: Djinn-Owned Session Compaction]]
- [[Roadmap]]