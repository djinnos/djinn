# Deferred chart contracts

Scripts in this directory are **real contracts that currently fail**, held out
of the gating suite because the defect they expose cannot be fixed from this
directory. `scripts/test-helm-chart-contracts.sh` globs
`deploy/helm/djinn/tests/*.sh` non-recursively, so nothing here runs in CI.

This is not a skip-list for flaky or inconvenient tests. Adding a script here
requires naming the exact defect and the exact change that unblocks it, below.
Removing the blocker means moving the script back up one directory in the same
change that fixes it.

## `log-collector-delivery.sh`

Blocked on three independent defects, all outside the chart-tests directory:

1. **`logCollector.bufferMiB` can never produce a loadable Vector config.**
   `templates/configmap-log-collector.yaml` renders the rotator sink's disk
   buffer as `max_size: bufferMiB * 1048576`, but Vector rejects any disk
   buffer below `268435488` bytes (256 MiB + 32 B):

   ```
   Sink "rotator": error occurred when building buffer: failed to build
   individual stage 0: parameter 'max_buffer_size' was invalid: must be
   greater than or equal to 268435488 bytes
   ```

   `values.schema.json` caps `bufferMiB` at 64 and `values.yaml` defaults it to
   64, so *every* value the schema admits is invalid. Fixing this means raising
   the schema bound **and** the `values.yaml` default together (and resizing the
   `vector-buffer` emptyDir to match); a schema-only change makes the shipped
   default fail validation. `values.yaml` was owned by another in-flight change
   when this was found, so the fix was left to that owner.

2. **The fixture needs a compiled `djinn-log-rotator`.** It sources
   `scripts/test-djinn-log-rotator-runtime.sh`, which falls back to
   `cargo build -p djinn-log-rotator`. The `Local Dev Contracts` job that runs
   the chart contracts has no Rust toolchain and a 15-minute budget, so this
   fixture needs either a prebuilt binary via `DJINN_LOG_ROTATOR_BIN` or a
   different CI job.

3. **`UID` is assigned as a plain shell variable.** Both this script and
   `scripts/test-djinn-log-rotator-runtime.sh` do `UID=550e8400-...`. That works
   under `dash` but aborts immediately under any `/bin/sh` backed by bash
   (`UID: readonly variable`), which is what most developer machines have.
