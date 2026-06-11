# Scripts

## What it is

Lightweight guard that flags Rust source files in `server/crates/**` and `server/src/**` exceeding the size guideline (~1,500 lines / ~50 KB).

## How to run locally

```sh
./scripts/check-file-size.sh
```

Example invocation:

```sh
MAX_LINES=1200 MAX_BYTES=45000 ./scripts/check-file-size.sh
```

Example output snippet:

```text
FAIL  server/crates/example/src/lib.rs  (1700 lines, 68030 bytes)
Found 1 oversized file(s) under MAX_LINES=1500 / MAX_BYTES=51200.
```

## Thresholds

| Variable | Default | Meaning |
| --- | ---: | --- |
| `MAX_LINES` | `1500` | Maximum line count before a file is oversized. |
| `MAX_BYTES` | `51200` | Maximum byte count before a file is oversized. |

Set either env var on the command line to override the default.

## Escape hatch

Put `// djinn:allow-oversize` on any line of a file to allow that file. Use only when the file is intentionally large and should not block CI.

## What's skipped

Generated paths are ignored: any path matching `**/generated/**` and any file matching `*.gen.*`.

## CI wiring

`.github/workflows/quality-gate.yml` runs `server-size-guard` when `needs.changes.outputs.server == 'true' && github.event_name != 'push'`.
