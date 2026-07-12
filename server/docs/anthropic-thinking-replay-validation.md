# Anthropic thinking replay validation

Operational close-out runbook for the deterministic core/provider/session/reply-loop coverage
introduced by proposal **io4p**, plus an opt-in manual live Anthropic multi-turn validation path.

**Scope**

- This document lists the exact repository-local commands reviewers can run to verify
  the recorded `parse → session persistence → next Anthropic request serialization` fixture
  and the provider-side regressions that guard the shared `ContentBlock` schema change.
- It provides a manual, non-blocking, credential-gated live Anthropic procedure for
  confidence when credentials and a thinking-capable model are available.
- CI remains deterministic and credential-free; no workflow or default test path is changed
  to require an `ANTHROPIC_API_KEY`.

---

## 1. Deterministic CI coverage (credential-free)

All commands below run from the repository root (`server/`). They exercise only recorded
or synthetic fixtures and local unit tests.

### 1.1 Core shared schema tests

The shared `ContentBlock` schema lives in `server/crates/djinn-core/src/message.rs` and is the
source of truth for persistence, compaction, and every provider serialization path.

```bash
cd server
cargo test -p djinn-core --lib message
```

What this covers: `ContentBlock` adjacently-tagged serde round-trip for `Text`, `ToolUse`,
`ToolResult`, `Image`, `Document`, `Thinking` (with optional `signature`), `RedactedThinking`,
`Unknown` passthrough, and `OpenAIReasoning`.

### 1.2 Anthropic provider replay and request-budget tests

The Anthropic format implementation is split under `server/crates/djinn-provider/src/provider/format/anthropic/`.

```bash
cd server
cargo test -p djinn-provider --lib anthropic
```

Focused sub-modules that are relevant to the io4p change:

```bash
# Streaming parse coverage for signed thinking, redacted thinking, unknown blocks, and tool-use ordering.
cargo test -p djinn-provider --lib format::anthropic::tests::streaming::test_indexed_thinking_redacted_unknown_and_tool_blocks

# Full parse → shared serde → Anthropic request round-trip fixture.
cargo test -p djinn-provider --lib format::anthropic::tests::replay_roundtrip

# Request-budget / reasoning-effort behavior (existing pre-io4p guard, must stay green).
cargo test -p djinn-provider --lib format::anthropic::tests::request::test_reasoning_effort

# Native assistant thinking replay guards (must emit native blocks, not empty text).
cargo test -p djinn-provider --lib format::anthropic::tests::request::test_build_request_replays
```

Key assertions that must pass:

- `signed_thinking_then_tool_use_replays_before_tool_result`: assistant content contains a
  native `{"type":"thinking","thinking":"...","signature":"..."}` block followed by the original
  `tool_use`, and the user `tool_result` continuation is present in the next message.
- `redacted_thinking_and_unknown_passthrough_round_trip`: redacted-thinking `data` and unknown
  passthrough fields survive `parse → serde → serialize` and emit native `redacted_thinking`
  and `type`-correct blocks.
- `replay_regression_empty_text_fallback_for_thinking_is_absent`: no `{"type":"text","text":""}`
  placeholder appears in the serialized assistant content; unsigned/empty-signature thinking
  is omitted instead.
- `test_reasoning_effort_*`: `thinking: { type: "enabled", budget_tokens }` is emitted only when
  `reasoning_effort` is set, is clamped below `max_tokens`, and forced `tool_choice` is skipped
  while thinking is enabled.

### 1.3 Real session/reply-loop persistence fixtures

The reply-loop persistence path serializes assistant messages to `session_messages` and reloads
them on the next turn. These tests verify that `ContentBlock` variants survive the database
serde round-trip inside a real `SlotContext`/`SessionMessageRepository`.

```bash
cd server
export DATABASE_URL="${TEST_POSTGRES_URL:-postgresql://postgres:postgres@localhost:5432/djinn_test}"
# Ensure the DB schema is present before running integration tests.
cargo sqlx prepare --workspace
```

> `cargo sqlx prepare --workspace` is only needed if the sqlx query cache is stale; the
> verification environment runs this automatically. Use it locally when a test fails with a
> sqlx metadata mismatch.

Run the reply-loop persistence tests that create real sessions and store messages:

```bash
cargo test -p djinn-slot --lib reply_loop::tests::proactive_compaction
```

This exercises `SessionMessageRepository::insert_messages_batch`, `load_conversation`, and the
serde path used by `serialize_message` / `flush_in_flight_turn` in `reply_loop/persistence.rs`.

For the broader set of reply-loop smoke tests that include tool-call continuation and
in-flight flushing:

```bash
cargo test -p djinn-slot --lib reply_loop_tests
```

What this covers: the same `ContentBlock` types that the Anthropic provider replay tests use
are persisted through the real DB repository path and reloaded into a `Conversation`.

### 1.4 Non-Anthropic regression tests (OpenAI / Google)

Because the shared `ContentBlock` schema added `Thinking`/`RedactedThinking`/`Unknown` variants,
OpenAI and Google serialization must continue to drop them rather than leak them as empty text.

```bash
cd server
# OpenAI chat-completions must drop Anthropic thinking/unknown blocks and not emit empty text.
cargo test -p djinn-provider --lib format::openai::tests::test_build_request_drops_anthropic_thinking_and_unknown_blocks

cargo test -p djinn-provider --lib format::openai::tests::test_build_request_all_internal_blocks_dropped_with_empty_assistant

# Google must also omit internal thinking/redacted/unknown blocks and remain native-shaped.
cargo test -p djinn-provider --lib format::google
```

### 1.5 Compaction regressions (shared schema consumer)

The compaction crate matches on `ContentBlock` variants and must handle the new thinking
variants without panicking or corrupting size accounting.

```bash
cd server
cargo test -p djinn-compaction --lib policy
```

---

## 2. Opt-in manual live Anthropic multi-turn validation

This procedure is **reviewer-only, non-blocking, and manual**. It requires a valid
`ANTHROPIC_API_KEY` and a thinking-capable Claude model chosen by the caller. It is not run in
CI and must not be added to any credential-consuming workflow.

### 2.1 Prerequisites

- `ANTHROPIC_API_KEY` exported in the environment.
- A thinking-capable model ID, e.g. `claude-sonnet-4-20250514` or another model that emits
  `thinking` / `redacted_thinking` content blocks. The model ID is chosen by the reviewer and
  passed explicitly; the runbook does not prescribe a default.
- The project builds the same tree that the deterministic tests above passed on.
- Network access to `api.anthropic.com` (or the Anthropic-compatible endpoint under test).

### 2.2 Recommended multi-turn prompt sequence

The goal is to observe a native `thinking` or `redacted_thinking` block followed by a `tool_use`,
then return a `tool_result`, and finally verify that the next request to Anthropic replays the
original thinking block(s) before the tool-use continuation.

1. Start a conversation with a single user message that asks the model to use a tool while
   reasoning aloud:

   > "Use the `shell` tool to run `pwd` and explain your reasoning."

2. Provide a tool definition for `shell` with a single `cmd` string parameter.

3. Enable thinking by setting `reasoning_effort` to `medium` or `high` on the provider config
   (or by selecting a model whose capability metadata defaults thinking on).

4. Run the first turn and capture the stream. Expected evidence from the streamed response:

   - One or more `content_block_start` / `content_block_delta` events with type `thinking` and
     a closing `signature_delta` that produces a non-empty `signature`.
   - OR a `redacted_thinking` block in place of the signed thinking block.
   - A `tool_use` block with the `shell` call (`cmd: "pwd"`).

5. Execute the tool locally and append the result as a user `tool_result` block.

6. Run the second turn against the same `Conversation` (after persistence round-trip if testing
   the full session path). Capture the serialized request body sent to Anthropic.

### 2.3 Expected replay evidence in the serialized next request

Inspect the `messages` array of the second-turn request body. The assistant message must contain:

```json
{
  "role": "assistant",
  "content": [
    { "type": "thinking", "thinking": "<the original reasoning text>", "signature": "<original signature>" },
    { "type": "tool_use", "id": "<tool id>", "name": "shell", "input": { "cmd": "pwd" } }
  ]
}
```

If the first turn emitted a redacted-thinking block, the assistant message must contain:

```json
{ "type": "redacted_thinking", "data": "<opaque original data>" }
```

The user message after the tool call must contain the matching `tool_result`:

```json
{
  "role": "user",
  "content": [
    { "type": "tool_result", "tool_use_id": "<tool id>", "content": [{"type":"text","text":"/workspace"}], "is_error": false }
  ]
}
```

What to look for in the request body:

- The assistant `content` array preserves the original order: thinking/redacted-thinking block(s)
  appear before the `tool_use` block.
- The `thinking` block includes the original `signature` value.
- There is **no** `{"type":"text","text":""}` placeholder where unsigned thinking would have been.
- There are no `signature` or `data` values in log output (see redaction guidance below).

### 2.4 Failure symptoms that indicate a regression

| Symptom | Likely cause |
|---|---|
| Anthropic returns `400 invalid_request_error` about missing `signature` or malformed `thinking` | Signed thinking was dropped or the `signature` field was not preserved across the persistence round-trip. |
| Anthropic returns `400` about `tool_use` without a matching `tool_result` | The `tool_use` id changed or the user `tool_result` was not appended. |
| Assistant content contains `{"type":"text","text":""}` | The empty-text fallback was reintroduced for unsigned thinking. |
| Redacted thinking `data` is missing or changed | The opaque blob was not stored verbatim or was lost during serde. |
| Model response ignores prior reasoning | The replayed thinking blocks were not placed before the `tool_use` continuation. |

---

## 3. Secret-safe logging and inspection

Do not log the full Anthropic request body, response body, or `signature`/`data` values to
untrusted storage. When inspecting a live turn:

- Log only block **types** and **positions** (e.g. `message[0] = assistant, content[0].type = thinking`).
- Verify `signature` and `data` presence by checking `value.is_string()` and `!value.is_empty()`,
  not by printing the value.
- If you must persist evidence for a review, redact `signature` and `redacted_thinking.data`:

```rust
fn redact_for_log(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        if obj.get("type").and_then(|t| t.as_str()) == Some("thinking") {
            if let Some(sig) = obj.get_mut("signature") {
                *sig = serde_json::json!("<redacted>");
            }
        }
        if obj.get("type").and_then(|t| t.as_str()) == Some("redacted_thinking") {
            if let Some(data) = obj.get_mut("data") {
                *data = serde_json::json!("<redacted>");
            }
        }
    }
}
```

- Never commit an `ANTHROPIC_API_KEY` value, a live request/response capture, or a thinking
  signature to the repository or to test fixtures.

---

## 4. Cleanup guidance after a live run

After completing the manual live validation:

1. Unset or rotate `ANTHROPIC_API_KEY` if it was exported in a shell session.
2. Remove any local capture files, temporary JSON dumps, or tracing logs that may contain the
   key, signatures, or redacted-thinking data.
3. Confirm `git status` shows no untracked credential files or live-capture artifacts.
4. If the live run was used to update this runbook or a test fixture, ensure the committed
   artifact contains only deterministic, redacted data.

---

## 5. Summary of CI vs. live validation

| Concern | CI / deterministic | Manual live |
|---|---|---|
| `parse → shared serde → Anthropic serialize` | `djinn-provider` `replay_roundtrip` tests | Second live turn with real model |
| Session persistence round-trip | `djinn-slot` reply-loop persistence tests | Real session DB after tool continuation |
| Non-Anthropic regressions | `djinn-provider` `openai`/`google` tests | — |
| Reasoning-budget request behavior | `djinn-provider` `request` tests | First live turn with thinking enabled |
| Credential requirement | None | `ANTHROPIC_API_KEY` + chosen model |
| Gating status | Required | Optional reviewer confidence |

**Key guarantee:** the recorded fixtures and unit tests above remain green and credential-free.
The live procedure is only additional evidence when a human reviewer has Anthropic access and
chooses to run it.
