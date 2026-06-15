# Safe MiniMax Anthropic SSE capture

Use the `djinn-provider` example binary to capture raw Anthropic-compatible streaming `data:` frames from a MiniMax model into a local JSON artifact without writing credentials.

Dry-run request preview, no credentials or network required:

```sh
cd server
cargo run -p djinn-provider --example capture_minimax_sse -- \
  --dry-run \
  --base-url https://api.minimax.io/anthropic/v1 \
  --model MiniMax-M3 \
  --reasoning-effort low \
  --output /var/tmp/minimax-anthropic-sse-dry-run.json
```

Live capture, for an operator with credentials:

```sh
cd server
MINIMAX_API_KEY='...' cargo run -p djinn-provider --example capture_minimax_sse -- \
  --base-url https://api.minimax.io/anthropic/v1 \
  --model MiniMax-M3 \
  --reasoning-effort low \
  --output /var/tmp/minimax-anthropic-sse-capture.json
```

Inputs may also be supplied through environment variables:

- `MINIMAX_BASE_URL` (default: `https://api.minimax.io/anthropic/v1`)
- `MINIMAX_MODEL` (default: `MiniMax-M3`)
- `MINIMAX_API_KEY`
- `MINIMAX_AUTH_HEADER` (optional; when unset, auth is `Authorization: Bearer <key>`)
- `MINIMAX_CAPTURE_PROMPT`
- `MINIMAX_REASONING_EFFORT` (`minimal`, `low`, `medium`, or `high`)
- `MINIMAX_MAX_TOKENS` (default: `4097`)
- `MINIMAX_CAPTURE_OUTPUT`

The capture path reuses the provider crate's Anthropic request assembler, so it sends the same `/v1/messages` shape as normal Anthropic-format providers. Supplying `--reasoning-effort` enables an explicit Anthropic `thinking` block and the artifact records `request.thinking_requested`, `request.reasoning_effort`, and `request.thinking_budget_tokens`.

The artifact contains:

- sanitized request metadata (`model`, `base_url`, URL path, `max_tokens`, stream flag, thinking metadata, redacted auth/header summary)
- `data_frames[]`, the raw payloads from streamed SSE `data:` lines, excluding `[DONE]`

The utility never writes bearer/API-key values or environment dumps. Known secret-bearing headers and the configured credential are redacted from metadata and from captured frames in case a gateway echoes them. Keep artifacts local unless they have been reviewed for provider-sensitive content.
