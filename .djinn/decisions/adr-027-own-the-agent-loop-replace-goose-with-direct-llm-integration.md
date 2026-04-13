---
title: ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration
type: adr
tags: ["adr","agent-loop","providers","langfuse"]
---

# ADR-027: Own the Agent Loop — Replace Goose with Direct LLM Integration

## Status: Accepted (supersedes ADR-008)

Date: 2026-03-09

## Context

ADR-008 chose Goose as the agent harness for V1, providing session management, multi-provider support, context compaction, and platform tools. After completing all V1 phases and operating the system, the cost-benefit has shifted:

**What Goose provides that Djinn actually uses (~20% of surface area):**
- LLM API calls via reqwest + SSE parsing
- Provider abstraction for ~20 providers
- Auto-compaction (generic summarization at 80% threshold)
- Session persistence (separate SQLite DB)
- Platform tools (developer write/edit, analyze, todo, summon)

**What Djinn already owns:**
- All prompt rendering (Goose just gets a string via `extend_system_prompt`)
- All custom tools (task_show, memory_read, shell, etc.) — Goose is a passthrough
- Tool result dispatch and handling
- Session management (duplicate sessions table in Djinn's DB)
- Provider/model selection (health tracker, models.dev catalog, context window injection)
- The entire reply loop (consuming Goose's stream, handling cancellation, tracking tokens)
- Coordination, lifecycle, pause/resume

**What Djinn cannot control with Goose:**
- **Compaction quality** — Goose's compaction is a black-box generic summarization. No awareness of task ACs, no structured preservation, no role-aware strategies. ADR-018 already decided Djinn should own compaction but still assumed Goose underneath.
- **Mid-session nudging** — Cannot inject context updates during compaction
- **Session storage** — Forced into a second SQLite DB for Goose sessions
- **Provider behavior** — Working around Goose's context limit handling, injecting values Goose doesn't know about

The replacement cost is moderate: the LLM HTTP client layer is well-understood (reqwest + SSE), and most providers speak the OpenAI-compatible format.

## Decision

**Replace the Goose library dependency with a Djinn-owned agent loop using direct LLM API calls.**

### Port Strategy: Fork-in-Place

Copy relevant Goose source files into the Djinn codebase, then modify. This preserves working code while enabling incremental customization. Specifically:

1. **Copy** Goose's provider HTTP/SSE layer, reply loop, compaction, and OAuth flows
2. **Adapt** to use Djinn's DB (single SQLite), credential vault, and existing session model
3. **Remove** Goose crate dependency once all functionality is ported
4. **Modify** compaction and other areas incrementally after the port is stable

### Provider Architecture: 3 Format Families

Instead of porting 20+ individual providers, implement 3 format handlers:

| Format | Covers | Auth |
|--------|--------|------|
| **OpenAI-compatible** | OpenAI, xAI, Groq, DeepSeek, Together, OpenRouter, LiteLLM, Tetrate, GitHub Copilot, Azure | Bearer token / OAuth |
| **Anthropic** | Anthropic (direct) | API key (`x-api-key` header) |
| **Google** | Google AI Studio, Vertex AI | API key / GCP auth |

The models.dev catalog already handles provider/model discovery. New providers that speak OpenAI-compatible format work automatically — just a different base URL + API key.

### OAuth Flows Ported

- **ChatGPT Codex**: Authorization Code + PKCE (~400 LOC). Browser redirect to `auth.openai.com`, local callback server on `localhost:1455`, JWT account ID extraction. Post-auth: OpenAI-compatible with `chatgpt.com/backend-api/codex` base URL + `chatgpt-account-id` header.
- **GitHub Copilot**: Device Code flow (~300 LOC). POST to GitHub, poll for token, exchange for Copilot API token at `api.github.com/copilot_internal/v2/token`. Dynamic API endpoint from token response.
- **Databricks**: OAuth device code (if needed later).

### Platform Tools

- **Keep**: `developer` (write/edit) — ported into Djinn's extension system
- **Drop**: `todo` (persistent checklist), `summon` (subagent delegation), `analyze` (tree-sitter — shell + tree command suffices)

### Session Consolidation

Eliminate Goose's separate `~/.djinn/sessions/sessions.db`. Store all session data — including conversation messages — in Djinn's main `~/.djinn/djinn.db`. The existing `sessions` table already tracks task_id, model_id, agent_type, tokens_in/out. Add message storage columns/table for conversation history.

### Token Counting

Use token counts from LLM API responses when available (OpenAI and Anthropic both return `usage` in responses). Fall back to tiktoken for providers that don't report usage or for pre-request estimation.

### Compaction

Copy Goose's existing compaction logic as-is for the initial port (80% threshold, progressive tool-response filtering, LLM summarization). This maintains behavioral parity. Customize later per ADR-018's design (AC-aware summaries, role-specific strategies, structured preservation).

### Observability: Helicone Proxy (Dev Mode)

Use Helicone as a local development proxy for full LLM observability. In dev mode, LLM API calls route through a self-hosted Helicone instance, capturing every request and response — system prompts, tool definitions, each turn, completions, token counts — with zero instrumentation code.

**Dev mode** (local Helicone running):
- Base URLs swap: `api.anthropic.com` → `localhost:<helicone_port>`, etc.
- Single header added: `Helicone-Auth: Bearer <local_key>`
- Optional metadata headers: `Helicone-Property-TaskId`, `Helicone-Property-AgentType`, `Helicone-Session-Id`

**Release mode** (production):
- Base URLs point to real provider APIs directly
- No Helicone dependency, no proxy in the critical path
- No observability overhead

Implementation: the provider factory reads a `dev_proxy_url` setting. When set, all providers use it as base URL prefix. When unset (release), providers use their native API URLs. This is a single `if` in provider construction — no Helicone-specific code beyond URL substitution and the auth header.

Self-hosted Helicone runs as `docker compose up` locally (4 containers: Postgres, ClickHouse, MinIO, Jawn). Not deployed to production.

### HTTP/SSE Stack

Use the same primitives Goose uses internally:
- `reqwest` for HTTP client with connection pooling
- `eventsource-stream` or manual `FramedRead` + `LinesCodec` for SSE parsing
- Provider-specific SSE event parsers (OpenAI `data: {"choices":[...]}`, Anthropic `message_start`/`content_block_delta`/`message_stop`, Google format)

## Consequences

**Positive:**
- Full control over compaction — AC-aware, role-specific, structured preservation
- Single database — no separate Goose SQLite, simpler operations
- Smaller dependency tree — remove Goose crate (large, pulls in many transitive deps)
- Helicone proxy for dev — full request/response observability with zero instrumentation code
- Provider flexibility — easy to add new OpenAI-compatible providers via models.dev catalog
- No abstraction fighting — context limits, token tracking, session lifecycle all owned
- Can customize reply loop behavior per agent type
- OAuth flows are self-contained and portable

**Negative:**
- Significant initial porting effort (estimated 2500-3500 LOC of new/adapted code)
- Must maintain SSE parsers for 3 API formats (OpenAI, Anthropic, Google)
- Lose automatic benefit of Goose upstream improvements
- OAuth flows need manual maintenance if providers change their endpoints
- Token counting fallback (tiktoken) adds a dependency

## Alternatives Considered

1. **Keep Goose, disable compaction only** — ADR-018's original approach. Insufficient: still fighting the abstraction on sessions, providers, and the reply loop.
2. **Use rig-core framework** — Too opaque. Owns the agent loop, which is what we want to own. Pre-1.0 with breaking changes.
3. **Use async-openai + async-anthropic crates** — Viable for the HTTP layer, but adds two dependencies for what's ~500 lines of reqwest + SSE parsing. The fork-in-place approach gives us proven code with no new crate dependencies.
4. **Use genai crate** — Multi-provider client but tool calling not implemented. Dealbreaker.

## Relations

- [[decisions/adr-008-agent-harness-—-goose-library-over-summon-subprocess-spawning|ADR-008: Agent Harness — Goose Library over Summon Subprocess Spawning]] — SUPERSEDED by this ADR
- [[decisions/adr-018-djinn-owned-session-compaction|ADR-018: Djinn-Owned Session Compaction]] — Compaction strategy now fully owned, not wrapping Goose
- [[decisions/adr-010-session-cost-tracking-—-per-task-token-metrics|ADR-010: Session Cost Tracking — Per-Task Token Metrics]] — Token metrics now sourced directly from API responses
- [[decisions/adr-015-session-continuity-resume|ADR-015: Session Continuity & Resume]] — Session storage moves to single DB
- [[Roadmap]] — New phase for this work


- Task lctb: Djinn-native message types and conversation model
- Task 8o1w: Provider HTTP layer with 3 format families
- Task dsb7: Developer tools port (write/edit)
- Task a87g: Session message storage migration
- Task ty9u: Reply loop — stream, tool dispatch, continue
- Task sbue: OAuth flows — Codex PKCE and Copilot device code
- Task zih5: Compaction — 80% threshold with LLM summarization
- Task g7qy: Lifecycle rewiring — replace Goose in slot lifecycle
- Task qmcl: Goose crate removal and cleanup