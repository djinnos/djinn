# Production-image memory workload driver

`memory_workload.pl` is an operator-side harness. It consumes
`../fixtures/manifest.json` and calls only supplied commands for the landed
canonical graph install, board scanner, and generation/restart inspection; it
does not create a production API or server instrumentation.

## Production invocation

Set the endpoints and credentials outside the command line (the token is never
written to JSONL), and supply commands that emit JSON:

```sh
export DJINN_METRICS_URL=https://server.example/metrics
export DJINN_GALAXY_URL=https://server.example/api/galaxy/artifact
export DJINN_GALAXY_TOKEN=... # never pass this as a flag
export DJINN_GRAPH_INSTALL_COMMAND='operator-installed graph loader command'
export DJINN_BOARD_SCAN_COMMAND='operator command invoking the landed board scanner; emits {"pages":40}'
export DJINN_GRAPH_GENERATION_COMMAND='operator command emitting {"generation":"..."}'
export DJINN_RESTART_COUNTER_COMMAND='operator command emitting {"restarts":0}'
# Emits observed seed/checksums plus graph, board_health, and galaxy_artifact identity.
export DJINN_FIXTURE_INSPECTION_COMMAND='operator fixture inspection command'
perl server/docs/operational/memory-sustainability/driver/memory_workload.pl \
  --output /var/tmp/memory-sustainability/raw.jsonl
```

Defaults are the release protocol: T0 1800 seconds with no graph, graph-install
peak, T1 900 seconds with the same graph, a 7200-second burst with 300-second
board ticks and 100 sequential 200/304 requests, then T2 after 300 seconds.
Before mutation it verifies commands, observed fixture identity, cgroup signals,
metrics, a 40-page board pass, and 200/304 galaxy responses. It rejects malformed
`memory.current`/`memory.events`, OOM/restart deltas, generation replacement, and
a missing resident slot; JSONL includes observed fixture identity and warm/server peaks.

Evidence is append-only `memory-sustainability-raw/v1` JSONL. The final filename
is created atomically only after every phase completes. Failures and SIGINT/SIGTERM
leave `<output>.partial` with samples and a failure/interruption record. Tokens
are intentionally absent from metadata and request records.

## Fast fixture-backed smoke

This uses the exact state machine and parsers, but replaces commands, HTTP, and
cgroup reads with deterministic fixture-shaped values. Overrides make it take
seconds; they do not alter the checked-in production defaults recorded in the
run metadata.

```sh
perl server/docs/operational/memory-sustainability/driver/memory_workload.pl \
  --fake --profile smoke --output /var/tmp/memory-smoke.jsonl \
  --t0-seconds 0 --t1-seconds 0 --burst-seconds 0 --t2-seconds 0 \
  --request-count 6
perl server/docs/operational/memory-sustainability/driver/tests/smoke.t
```
