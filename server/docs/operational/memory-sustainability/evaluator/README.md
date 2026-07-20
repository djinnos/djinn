# Memory-sustainability offline evaluator

This subtree defines the release-gate input (`raw-schema.json`), deterministic evaluator (`evaluate.pl`), and synthetic unit tests. It consumes the versioned fixture manifest at `../fixtures/manifest.json`; it does not generate server telemetry or contact infrastructure.

## Input and command

The input wrapper has a required `candidate` raw run and an optional `pre_change_diagnostic` raw run. Both use `memory-sustainability-raw/v1` and must carry the same identity in every evidence record: root `run_id` / `candidate_image_id`, then record `run_id` / `image_id`. Values are **integral bytes** (never strings, MiB, or floating point) except board `duration_ms`. Required sample phases are `T0`, `graph_install`, `T1`, `burst`, and `T2`.

```sh
perl server/docs/operational/memory-sustainability/evaluator/evaluate.pl --input raw.json \
  --json-out evaluation.json --report-out evaluation.md
```

The JSON output has stable sorted keys, retains the original raw run beneath each result, includes its canonical SHA-256, and gives every gate observed value, threshold, units, JSON-pointer evidence, and `pass`/`fail`/`error` status. The Markdown rendering is generated from that same result.

A candidate can pass only when the input is valid and all gates pass:

- server and warm-job observed peaks are each `<= 3.5 GiB` in the required `4 GiB` cgroup;
- maximum request RSS delta is `<= 32 MiB`;
- every board pass is `<= 120000 ms`;
- OOM-kill and restart counters are monotonic and have zero T0-to-T2 delta;
- all required samples retain exactly one nonempty graph generation;
- T2 RSS delta is `<= max(128 MiB, floor(10% of T1 RSS))`;
- T2 jemalloc retained delta is `<= 256 MiB`.

Unsupported versions, malformed units, missing/duplicate phases, missing signals, mixed run/image identities, generation drift, and counter regressions are errors, never a success. The optional pre-change raw run is rendered as a **non-release-gating diagnostic**. Its result is intentionally separate and cannot replace or mask a candidate failure.

Run only the local test module:

```sh
perl server/docs/operational/memory-sustainability/evaluator/tests/test_evaluate.pl
```
