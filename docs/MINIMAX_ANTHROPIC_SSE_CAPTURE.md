# Safe MiniMax Anthropic SSE capture

Use the `djinn-provider` example binary to capture raw Anthropic-compatible streaming `data:` frames from a MiniMax model into a local JSON artifact without writing credentials. This runbook is for proposal `60mp` / epic `aiu0` characterization: determine whether MiniMax emits reasoning as Anthropic structured thinking deltas, leaks inline `<think>` tags in text deltas, or emits no observable reasoning.

The worker does **not** need live MiniMax credentials to satisfy this documentation path. Operators with credentials can run the live capture later and paste the sanitized result checklist below into the relevant task/proposal notes.

## Output location and artifact policy

Write capture artifacts outside the repository by default, for example:

- dry-run preview: `/var/tmp/minimax-anthropic-sse-dry-run.json`
- live capture: `/var/tmp/minimax-anthropic-sse-capture.json`
- optional reviewed/sanitized copy for sharing: `docs/artifacts/minimax-anthropic-sse/<YYYY-MM-DD>-capture-redacted.json`

Do **not** commit raw live artifacts unless they have been manually reviewed and reduced to provider-safe, prompt-safe content. A committed artifact must not contain API keys, bearer tokens, organization identifiers, private prompts, customer data, or full private completions. Prefer committing only the checklist/result summary in this document or in the relevant proposal/task comment.

## Dry-run request preview

Dry-run mode writes the same sanitized artifact envelope and request metadata without credentials or network access. Use it to confirm the command, request shape, output path, and `thinking` metadata before a live call:

```sh
cd server
cargo run -p djinn-provider --example capture_minimax_sse -- \
  --dry-run \
  --base-url https://api.minimax.io/anthropic/v1 \
  --model MiniMax-M3 \
  --reasoning-effort low \
  --output /var/tmp/minimax-anthropic-sse-dry-run.json
```

Expected dry-run properties:

- `request.provider_format` is `anthropic`
- `request.path` points at the Anthropic-compatible `/v1/messages` endpoint
- `request.stream` is `true`
- `request.thinking_requested` is `true` when `--reasoning-effort` is supplied
- `request.reasoning_effort` records the requested tier (`minimal`, `low`, `medium`, or `high`)
- `request.thinking_budget_tokens` records the request-side thinking budget selected by the provider request builder
- `data_frames` is empty because dry-run mode does not contact MiniMax

## Live capture command

Run a live capture only from an operator environment that is allowed to call MiniMax. Supply credentials explicitly through env or args; do not rely on shell history containing reusable secrets.

Preferred env-based invocation:

```sh
cd server
MINIMAX_API_KEY='...' \
MINIMAX_CAPTURE_PROMPT='Reply with one short sentence. If thinking is available, use the provider structured thinking stream rather than inline <think> tags.' \
cargo run -p djinn-provider --example capture_minimax_sse -- \
  --base-url https://api.minimax.io/anthropic/v1 \
  --model MiniMax-M3 \
  --reasoning-effort low \
  --max-tokens 4097 \
  --output /var/tmp/minimax-anthropic-sse-capture.json
```

Equivalent explicit-arg form:

```sh
cd server
cargo run -p djinn-provider --example capture_minimax_sse -- \
  --base-url https://api.minimax.io/anthropic/v1 \
  --model MiniMax-M3 \
  --api-key '...' \
  --reasoning-effort low \
  --max-tokens 4097 \
  --prompt 'Reply with one short sentence. If thinking is available, use the provider structured thinking stream rather than inline <think> tags.' \
  --output /var/tmp/minimax-anthropic-sse-capture.json
```

If MiniMax requires a nonstandard auth header, provide it explicitly:

```sh
MINIMAX_API_KEY='...' \
MINIMAX_AUTH_HEADER='X-Api-Key' \
cargo run -p djinn-provider --example capture_minimax_sse -- \
  --model MiniMax-M3 \
  --reasoning-effort low \
  --output /var/tmp/minimax-anthropic-sse-capture.json
```

## Supported inputs

Inputs may be supplied through environment variables or CLI flags. CLI flags override env defaults.

| Env | Flag | Notes |
| --- | --- | --- |
| `MINIMAX_BASE_URL` | `--base-url URL` | Default: `https://api.minimax.io/anthropic/v1` |
| `MINIMAX_MODEL` | `--model MODEL` | Default: `MiniMax-M3` |
| `MINIMAX_API_KEY` | `--api-key KEY` | Required for live capture; omitted for `--dry-run` |
| `MINIMAX_AUTH_HEADER` | `--auth-header NAME` | Optional; when unset, auth is `Authorization: Bearer <key>` |
| `MINIMAX_CAPTURE_PROMPT` | `--prompt TEXT` | Keep prompts synthetic and non-private |
| `MINIMAX_REASONING_EFFORT` | `--reasoning-effort TIER` | `minimal`, `low`, `medium`, or `high`; enables Anthropic `thinking` |
| `MINIMAX_MAX_TOKENS` | `--max-tokens N` | Default: `4097`; request builder clamps thinking budget below `max_tokens` |
| `MINIMAX_CAPTURE_OUTPUT` | `--output PATH` | Use `/var/tmp/...` for raw local captures |
| n/a | `--header 'Name: value'` | Extra provider header; secret-like headers are redacted in output |
| n/a | `--dry-run` | Writes sanitized request metadata only; no API key/network required |

## Artifact format

The capture path reuses the provider crate's Anthropic request assembler, so it sends the same `/v1/messages` shape as normal Anthropic-format providers. Supplying `--reasoning-effort` enables an explicit Anthropic `thinking` block and the artifact records `request.thinking_requested`, `request.reasoning_effort`, and `request.thinking_budget_tokens`.

The JSON artifact has this shape:

```json
{
  "artifact_version": 1,
  "created_at": "2026-06-15T00:00:00Z",
  "request": {
    "provider_format": "anthropic",
    "model": "MiniMax-M3",
    "base_url": "https://api.minimax.io/anthropic/v1",
    "path": "/v1/messages",
    "max_tokens": 4097,
    "stream": true,
    "thinking_requested": true,
    "reasoning_effort": "low",
    "thinking_budget_tokens": 1024,
    "auth": { "kind": "bearer", "redacted": true },
    "headers": {}
  },
  "data_frames": [
    {
      "index": 0,
      "data": "{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}"
    }
  ]
}
```

`data_frames[].data` contains the raw JSON payload from each streamed SSE `data:` line, excluding `[DONE]`. The utility never writes bearer/API-key values or environment dumps. Known secret-bearing headers and the configured credential are redacted from metadata and from captured frames in case a gateway echoes them. Keep artifacts local unless they have been reviewed for provider-sensitive content.

## Fixture-backed classification outcomes

`server/crates/djinn-provider/src/provider/capture.rs` contains the fixture-backed helper `classify_anthropic_thinking_stream`, which classifies the capture artifact's `data_frames` into exactly one of these serialized outcomes:

| Outcome | Evidence in `data_frames[].data` | Meaning |
| --- | --- | --- |
| `structured_thinking` | Anthropic structured thinking shape, for example `content_block.type = "thinking"` or `delta.type = "thinking_delta"` | MiniMax is using the Anthropic structured thinking stream. Existing structured parsing in `server/crates/djinn-provider/src/provider/format/anthropic/streaming.rs` is the relevant downstream path. |
| `inline_think_tags` | No structured thinking frame was observed, but a `delta.type = "text_delta"` frame contains `<think>` or `</think>` text | MiniMax is leaking reasoning in ordinary text deltas. Downstream implementation must consider fallback extraction. |
| `no_reasoning_observed` | Neither structured thinking nor inline `<think>` tags were observed | The capture did not show reasoning output. Request-side `thinking` enablement and existing parsing are sufficient unless later captures show inline leakage. |

The helper is intentionally offline/test-facing; it does not change runtime completion behavior. Its synthetic tests cover structured `thinking_delta`, inline `<think>...</think>` text, and ordinary no-reasoning text without MiniMax credentials or network access.

For a live operator result, classify by either using the helper in a small local Rust harness/test or by manually inspecting `data_frames[].data` with the same rules above. Record only the outcome and sanitized observations in the checklist below.

## Result checklist/template

Copy this template into the relevant task/proposal note after a reviewed live capture. Do not paste secrets, raw private prompts, or unreviewed full frame contents.

```md
# MiniMax Anthropic thinking stream capture result

- Date/time (UTC):
- Operator/environment:
- Command mode: live capture / dry-run only
- Artifact path: /var/tmp/minimax-anthropic-sse-capture.json
- Reviewed/sanitized artifact committed? no / yes: <path>
- Base URL: https://api.minimax.io/anthropic/v1
- Model:
- Reasoning effort requested: minimal / low / medium / high / none
- Max tokens:
- `request.thinking_requested`: true / false
- `request.thinking_budget_tokens`:
- Prompt summary: synthetic minimal prompt; no private prompt content recorded
- Secret handling checked:
  - [ ] API key/bearer token absent from artifact
  - [ ] Secret-bearing headers redacted
  - [ ] No full private prompt or private completion content committed
  - [ ] Artifact kept in `/var/tmp` or committed only after manual sanitization

## Classification

Choose exactly one:

- [ ] `structured_thinking`
  - Evidence: observed `content_block.type = "thinking"` and/or `delta.type = "thinking_delta"` in `data_frames[].data`.
- [ ] `inline_think_tags`
  - Evidence: observed `<think>` or `</think>` in a text delta (`delta.type = "text_delta"`) and no structured thinking frame took precedence.
- [ ] `no_reasoning_observed`
  - Evidence: no structured thinking frame and no inline `<think>` tags in text deltas.

Sanitized evidence notes:

- Frame indexes inspected:
- Minimal redacted excerpts, if safe:
- Classification method: `classify_anthropic_thinking_stream` / manual inspection using runbook rules

## Downstream decision for proposal 60mp

- [ ] If classification is `inline_think_tags`: the proposal 60mp implementation epic must add fallback extraction for inline `<think>`-style reasoning in text deltas unless it explicitly defers that work with written rationale.
- [ ] If classification is `structured_thinking` or `no_reasoning_observed`: request-side `thinking` enablement plus the existing Anthropic structured parsing path is sufficient for proposal 60mp; no inline fallback is required from this evidence.

Decision/rationale recorded at:
```

## Downstream fallback decision rule

For proposal `60mp`, the downstream implementation decision is determined by the classified capture result:

1. **`inline_think_tags` observed:** the implementation epic for proposal `60mp` must add fallback extraction for inline `<think>`-style reasoning in text deltas unless it explicitly defers the fallback with written rationale. Inline tags mean reasoning is arriving as ordinary assistant text, so relying only on Anthropic structured `thinking_delta` parsing would leak or mishandle reasoning.
2. **`structured_thinking` observed:** request-side `thinking` enablement plus the existing Anthropic structured stream parser is sufficient. `server/crates/djinn-provider/src/provider/format/anthropic/streaming.rs` already parses structured `thinking_delta` into `StreamEvent::Thinking`.
3. **`no_reasoning_observed` observed:** request-side `thinking` enablement plus existing structured parsing is sufficient for the implementation wave unless a later capture shows inline leakage. No inline fallback is required solely because reasoning was absent from this capture.

The request side already emits Anthropic `thinking: { type: "enabled", budget_tokens }` when `ProviderConfig.reasoning_effort` is set, and this capture command records whether that request-side enablement was present. The purpose of this runbook is only to document MiniMax's observed stream shape and the fallback decision; it does not require the docs worker to make a live MiniMax call.
