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
perl server/docs/operational/memory-sustainability/driver/memory_workload.pl \
  --output /var/tmp/memory-sustainability/raw.jsonl
```

Defaults are the release protocol: T0 1800 seconds with no graph, graph-install
peak, T1 900 seconds with the same graph, a 7200-second burst with 300-second
board ticks and 100 sequential 200/304 requests, then T2 after 300 seconds.
The driver requires cgroup v2 `memory.max == 4294967296`, all process/jemalloc
and canonical-slot gauges, an initially empty graph slot, manifest graph counts
and bytes, a stable generation, 40-page board results, ETags for 200 responses,
and unchanged restart counter. It samples cgroup `memory.current`/`memory.events`
and all required metrics at each phase boundary.

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
