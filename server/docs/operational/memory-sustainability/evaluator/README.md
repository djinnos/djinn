# Memory-sustainability offline evaluator

This subtree defines the versioned release-gate input (`raw-schema.json`), driver-JSONL adapter (`adapt_driver_jsonl.pl`), deterministic evaluator (`evaluate.pl`), and synthetic tests. It consumes the fixture contract at `../fixtures/manifest.json`; it neither collects nor adds server telemetry.

## Input and command

The wrapper has a required `candidate` run and optional `pre_change_diagnostic` run. A run is an append-only `samples` stream plus append-only route and board evidence. Each sample carries a stable ID/timestamp, cgroup current usage and OOM events, process/anonymous RSS, the landed `djinn_jemalloc_{allocated,resident,retained}_bytes` values, and canonical graph-slot presence/size/node/edge measurements. Routes retain status, ETag, latency, and the driver-collected RSS immediately before and after that route request; board evidence retains page count and duration. Optional fixture-manifest and immutable evidence references preserve provenance.

```sh
perl server/docs/operational/memory-sustainability/evaluator/adapt_driver_jsonl.pl \
  --candidate-raw candidate/driver-raw.jsonl --candidate-image 'registry.example/djinn-server@sha256:<digest>' \
  --candidate-fixture-manifest candidate/fixture-manifest.json --output evaluator-input.json
perl server/docs/operational/memory-sustainability/evaluator/evaluate.pl --input evaluator-input.json \
  --json-out evaluation.json --report-out evaluation.md
```

The adapter is the required non-hand-editing bridge from driver JSONL to the evaluator wrapper. It validates a finalized successful collection, carries the driver run ID and supplied immutable image digest into every derived record, preserves the graph-install and final server peaks, requires the per-request route RSS measurements, parses collected timestamps (including the driver’s UTC format), and records SHA-256 references to the raw JSONL and fixture manifest. Add the three `--diagnostic-*` arguments together to retain an optional pre-change diagnostic run.

All values are JSON integers, never byte strings or display units. The evaluator rejects forward/unsupported schema versions, malformed wrapper roots and values, mixed identities, missing signals/phases, ambiguous T0/graph-install/T1/T2 anchors, and generation drift. It derives peaks from **every** sample in the stream and retains the supplied raw run in the stable, canonically-keyed JSON result. Evidence pointers use actual array indices.

T0 is required to prove no graph (`graph_generation_id: null`, `graph_slot_present: false`). Both fields are strictly validated: `graph_slot_present` must be a JSON boolean (not null, string, or number) and exactly `false` at T0. The installed graph generation must be nonempty and unchanged from graph-install through T2. Candidate gates are server/warm peaks `<= 3.5 GiB` in exactly 4 GiB, route RSS delta `<= 32 MiB`, every board duration `<= 120000 ms`, zero monotonic OOM/restart delta, T2 RSS delta `<= max(128 MiB, floor(10% of T1))`, and T2 retained delta `<= 256 MiB`.

The pre-change image is always rendered as a separately labeled diagnostic table with the same observed/threshold/unit/evidence details. It cannot affect the candidate release status.

```sh
perl server/docs/operational/memory-sustainability/evaluator/tests/test_evaluate.pl
```
