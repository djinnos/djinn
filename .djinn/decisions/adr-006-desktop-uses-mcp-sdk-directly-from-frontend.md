---
title: ADR-006: Desktop Uses MCP SDK Directly from Frontend
type: adr
tags: ["adr","desktop","mcp","tanstack-query"]
---

# ADR-006: Desktop Uses MCP SDK Directly from Frontend

## Status
Accepted

Date: 2026-03-05

## Context

Desktop currently has a typed frontend stack (TanStack Query + mutations) and a Tauri bridge for native OS integration. The server exposes domain functionality via MCP tools, not REST domain endpoints.

Routing all domain calls through custom Tauri IPC wrappers would add an extra translation layer that can drift from server tool contracts.

The desktop webview can reach the local daemon (`127.0.0.1`) and the project already has SSE handling for real-time updates.

## Decision

1. **Frontend calls MCP directly using `@modelcontextprotocol/sdk`.**
   - Build a shared MCP client in frontend TypeScript.
   - Use generated tool types from MCP schemas for request/response typing.

2. **Tauri IPC remains for native-only concerns.**
   - Keep IPC for operations such as server port discovery and OS dialogs.
   - Do not mirror MCP domain tools into Tauri commands.

3. **TanStack Query is the state/data layer for MCP tool usage.**
   - Queries and mutations call typed MCP wrappers.
   - SSE events invalidate/refetch relevant query keys.

## Consequences

**Positive:**
- No duplicated contract layer between frontend and server
- Faster feature delivery (tool appears in server -> callable in frontend)
- Cleaner cache model with TanStack Query + SSE invalidation
- Better type alignment with schema-driven generation

**Negative:**
- Frontend now owns MCP transport/session lifecycle concerns
- Browser-side MCP error handling must be robust and centralized
- If MCP contracts change, regenerated client types are required

## Relations
- [[decisions/adr-003-split-epic-and-task-mcp-tools-with-input-validation|ADR-003: Split Epic and Task MCP Tools with Input Validation]]
- [[ADR-005: Project-Scoped Epics, Tasks, and Sessions]]
- [[Roadmap]]