---
title: Agent Loop Port Scope
type: reference
tags: ["scope","reference","agent-loop"]
---

# Agent Loop Port Scope

Scope boundaries for Phase 12: Replace Goose with Djinn-owned agent loop. See [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]].

## In Scope

### Provider HTTP Layer
- `ApiClient` struct wrapping reqwest with connection pooling, auth strategies, timeouts
- `AuthMethod` enum: NoAuth, BearerToken, ApiKey, OAuth, Custom (async trait)
- 3 format implementations:
  - **OpenAI-compatible**: request builder + SSE parser (`data: {"choices":[...]}`)
  - **Anthropic**: request builder + SSE parser (`message_start`, `content_block_delta`, `message_stop`)
  - **Google**: request builder + SSE parser (Gemini format)
- Provider trait: `stream()` returning async message stream, `complete()` convenience method
- Provider factory: create provider from models.dev catalog entry (provider_id → format family → base URL + auth)

### Reply Loop
- Port Goose's `reply_internal()` logic: stream LLM response → handle tool calls → collect results → continue
- Tool dispatch: frontend tools (Djinn's existing extension system) + developer tools (write/edit)
- Cancellation via CancellationToken (existing pattern)
- Max turns limit (existing: 1000)
- Context length exceeded → trigger compaction → retry

### Compaction
- Copy Goose's existing compaction as-is:
  - 80% threshold check after each turn
  - Progressive tool-response filtering (0% → 10% → 20% → 50% → 100%)
  - LLM summarization call using session's own model
  - Summary replaces conversation, originals preserved for history
- Customization (AC-aware, role-specific) deferred to later iteration

### OAuth Flows
- **ChatGPT Codex**: Authorization Code + PKCE
  - PKCE challenge generation (nanoid + SHA256)
  - Browser redirect to `auth.openai.com/oauth/authorize`
  - Local callback server (axum, `localhost:1455`)
  - Code exchange at `auth.openai.com/oauth/token`
  - JWT account ID extraction
  - Token refresh via refresh_token grant
  - Token cache to disk (JSON file)
- **GitHub Copilot**: Device Code flow
  - POST `github.com/login/device/code` → user_code + verification_uri
  - Poll `github.com/login/oauth/access_token` (5s interval, 36 max)
  - Exchange GitHub token for Copilot API token at `api.github.com/copilot_internal/v2/token`
  - Dynamic API endpoint from token response
  - Spoofed VS Code user-agent headers

### Session Consolidation
- New `session_messages` table in Djinn's DB (replaces Goose's messages table)
  - Columns: id, session_id, role, content_json, tokens, metadata_json, created_at
  - Index on session_id for fast conversation loading
- Migration to add table
- Conversation load/save from Djinn's DB instead of Goose SessionManager
- Remove `~/.djinn/sessions/` directory and Goose's SessionManager dependency

### Developer Tools (write/edit)
- Port Goose's `developer` platform extension (write file, edit file)
- Expose as tool definitions in Djinn's extension system
- Drop: `tree` (shell suffices), `todo`, `summon`, `analyze`

### Token Counting
- Primary: extract from API response `usage` field (all 3 formats return it)
- Fallback: tiktoken for pre-request estimation or providers that don't report usage
- Feed into existing session token tracking (sessions.tokens_in/tokens_out)

### Helicone Dev Proxy
- Provider factory reads `dev_proxy_url` setting
- When set: base URLs swap to local Helicone, `Helicone-Auth` header added
- When unset: real provider API URLs, no proxy
- Optional metadata headers: `Helicone-Property-TaskId`, `Helicone-Property-AgentType`, `Helicone-Session-Id`
- Self-hosted via docker-compose (local dev only, not deployed to production)
- Zero instrumentation code — Helicone captures full request/response payloads automatically

### Message Types
- Port Goose's `Message`, `MessageContent` (Text, ToolRequest, ToolResult) types
- Or define Djinn-native equivalents that map cleanly to/from API formats
- Conversation struct for ordered message history

### Goose Removal
- Remove `goose` and `goose-mcp` from Cargo.toml
- Remove all `use goose::` imports
- Clean up type aliases (GooseAgent, GooseMessage, etc.)
- Delete `src/agent/mod.rs` init_session_manager and related Goose wiring

## Out of Scope

- **Custom compaction strategies** — copy Goose's as-is, customize in a later phase
- **Bedrock/Vertex AI providers** — can add later, OpenAI-compatible + Anthropic + Google covers the primary use cases
- **Local inference (llama.cpp)** — not a current need
- **Embeddings** — separate concern, not part of agent loop
- **MCP client for external tools** — agents use Djinn's tools directly, no external MCP servers needed
- **Permission manager** — Djinn's sandbox (ADR-013) handles this
- **Lead worker / model switching mid-session** — can add later
- **Vector search / RAG** — Phase 11 concern
- **Compaction prompt customization** — later iteration
- **OpenTelemetry integration** — Helicone proxy captures everything needed for dev; OTEL can be added later
- **Production observability** — Helicone is dev-only; production observability (if needed) is a separate concern

## Preferences

- Copy Goose source files first, adapt second — don't rewrite from scratch
- Keep the provider trait surface minimal: `stream()` + `complete()` + `name()`
- Use Djinn's existing `AppState` pattern for shared client/config access
- SSE parsing: prefer `eventsource-stream` crate over manual `FramedRead` if it simplifies code
- Helicone is dev-only — no production dependency, just a base URL toggle
- Token counting: trust API response over local estimation

## Estimation

- **Provider HTTP layer + 3 formats**: ~800-1000 LOC (adapted from Goose)
- **Reply loop**: ~500-700 LOC (adapted from Goose's `reply_internal`)
- **Compaction**: ~300-400 LOC (copied from Goose)
- **OAuth flows**: ~700 LOC (Codex ~400 + Copilot ~300)
- **Session consolidation**: ~200-300 LOC (migration + message repo)
- **Developer tools**: ~200 LOC (write/edit ported)
- **Helicone dev proxy support**: ~20 LOC (base URL toggle + header)
- **Message types**: ~200-300 LOC
- **Goose removal + rewiring**: ~negative LOC (deletion + import updates)
- **Total new/adapted**: ~3000-3500 LOC

## Relations

- [[ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration]]
- [[ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — superseded
- [[ADR-018: Djinn-Owned Session Compaction]] — compaction now fully owned
- [[Roadmap]] — Phase 12


- Task lctb: Djinn-native message types (~200-300 LOC)
- Task 8o1w: Provider HTTP layer + 3 formats (~800-1000 LOC)
- Task dsb7: Developer tools port (~200 LOC)
- Task a87g: Session message storage (~200-300 LOC)
- Task ty9u: Reply loop (~500-700 LOC)
- Task sbue: OAuth flows (~700 LOC)
- Task zih5: Compaction (~300-400 LOC)
- Task g7qy: Lifecycle rewiring (~500-700 LOC deletions/swaps)
- Task qmcl: Goose crate removal (negative LOC)