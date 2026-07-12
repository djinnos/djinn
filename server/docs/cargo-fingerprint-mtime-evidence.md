# Cargo 1.97 fingerprint mtimes do not record artifact reuse — operator evidence

**Status:** validated evidence for epic `w06b` (warm Cargo target fingerprint
staleness sweep).  
**Pinned toolchain:** `server/rust-toolchain.toml` resolves `channel = "stable"`
to `rustc 1.97.0` and `cargo 1.97.0` at the time of writing.  
**Pod-equivalent environment:** `CARGO_INCREMENTAL=1`, `RUSTC_WRAPPER=""` (see
`server/crates/djinn-k8s/src/job.rs` `warm_cache_env_vars` /
`task_run_cache_env_vars`).

## Why this matters

Epic `w06b` wants to know whether newest file mtime under a Cargo
`.fingerprint/<unit>` directory can safely identify stale artifacts in a warm
base. If the mtime is refreshed when an existing artifact is reused, a cutoff
rule could flag units that have not been reused recently. If not, destructive
selection based on that mtime would delete actively reused warm artifacts.

## What was tested

A reproducible Cargo workspace fixture in
`server/scripts/fixtures/cargo-fingerprint-mtime` contains:

- `ordinary` — ordinary path-dependency unit
- `pm` — `proc-macro = true` unit
- `app` — binary unit
- `app/build.rs` — build-script compile and run units

The harness `server/scripts/cargo-fingerprint-mtime-harness.sh` builds the
fixture in an isolated `CARGO_TARGET_DIR` with `CARGO_NET_OFFLINE=true`, records
exact Cargo/rustc versions and filesystem context, sleeps two seconds between
snapshots to avoid timestamp-resolution ambiguity, and compares every
fingerprint-file path and mtime across:

1. initial debug build
2. Fresh/no-op debug build
3. app-only rebuild with dependency reuse
4. initial release build
5. Fresh/no-op release build
6. explicit installed target triple (`x86_64-unknown-linux-gnu`) initial build
7. Fresh/no-op explicit-target build
8. mtime-preserving seeded-target reuse (copy `target` → `seeded-target` and
   build again)

## Results

| Scenario | Cargo classification | Did any `.fingerprint/<unit>` file change mtime? |
| --- | --- | --- |
| no-op debug build | `Fresh` | **No** |
| app-only rebuild | `Dirty app`, `Compiling app`; `Fresh ordinary`, `Fresh pm` | App unit + build-script *run* fingerprints refreshed; reused dependency and proc-macro stayed unchanged |
| no-op release build | `Fresh` | **No** |
| no-op explicit host target | `Fresh` | **No** |
| no-op seeded target copy | `Fresh` | **No** |

## Filesystem and toolchain context

Validation was run on an overlayfs volume with 4096-byte blocks and
nanosecond-resolution mtimes. The procedure deliberately compares full
nanosecond-resolution timestamps and inserts two-second sleeps so that any
observed "no change" is Cargo behavior, not timestamp-collision noise. The
recorded toolchain at validation time was:

```text
cargo 1.97.0 (c980f4866 2026-06-30)
rustc 1.97.0 (2d8144b78 2026-07-07)
installed target: x86_64-unknown-linux-gnu
```

You can reproduce this on any system with the same toolchain and installed
target:

```sh
server/scripts/cargo-fingerprint-mtime-harness.sh \
  --fixture server/scripts/fixtures/cargo-fingerprint-mtime \
  --target-dir /tmp/cargo-fingerprint-evidence
```

The script exits early and reports the installed target list if the requested
target triple is not present.

## Safety conclusion

Cargo 1.97.0 does **not** refresh fingerprint-file mtimes when it reports
units as `Fresh`. The newest mtime under `.fingerprint/<unit>` is a
compilation/materialization timestamp, not an access or reuse timestamp. Some
unit kinds (binary, build-script run result) refresh when Cargo reevaluates the
unit, but that still does not happen on pure reuse.

Therefore the predicate

```text
max(mtime of files in .fingerprint/<unit>) < cutoff
```

cannot safely distinguish "old and unused" from "old but repeatedly reused."
No finite cutoff eliminates the risk of deleting a frequently reused stable
dependency.

## Djinn policy consequence

Fingerprint-derived deletion is **disabled/report-only**. The warm-base
fingerprint sweep will inventory units and report their newest fingerprint
mtime, but it must never use that timestamp as a destructive staleness signal.
The only destructive bound for warm Cargo bases remains the guarded whole-base
idle/pressure eviction delivered by epic `8s2o`.

If a future pinned Cargo version changes this behavior, it must be validated
with this same fixture/harness across all relevant unit kinds before any
fingerprint-driven deletion is enabled. An alternative safe path would require an
independent durable usage journal or heartbeat under the existing per-base lock
(not introduced here).

## Related scope

- `server/rust-toolchain.toml` — pinned toolchain
- `server/crates/djinn-k8s/src/job.rs` — warm/task pod cache env construction
- `server/crates/djinn-coordinator/src/cargo_warm_base_gc.rs` — warm-base GC
  module (must keep delete mode disabled)
- `server/scripts/cargo-fingerprint-mtime-harness.sh` — reproducible harness
- `server/scripts/fixtures/cargo-fingerprint-mtime/` — fixture workspace
