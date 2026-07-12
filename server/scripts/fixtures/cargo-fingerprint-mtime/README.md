# Cargo 1.97 fingerprint-mtime validation harness

Reproducible fixture and script backing the technical finding in
[`research/technical/cargo-1-97-fingerprint-mtimes-do-not-record-artifact-reuse`](../../research/technical/cargo-1-97-fingerprint-mtimes-do-not-record-artifact-reuse).

The fixture creates a small Cargo workspace with four unit kinds:

- `ordinary` — ordinary path dependency
- `pm` — `proc-macro = true` crate
- `app` — binary with `build = "build.rs"`
- `app/build.rs` — build script

## Running the harness

```sh
server/scripts/cargo-fingerprint-mtime-harness.sh \
  --fixture server/scripts/fixtures/cargo-fingerprint-mtime \
  --target-dir /tmp/cargo-fingerprint-evidence
```

The harness:

1. Records `rustc --version`, `cargo --version`, installed targets, filesystem
   type, and block size.
2. Builds the fixture with `CARGO_INCREMENTAL=1`, `RUSTC_WRAPPER=""`, and
   `CARGO_NET_OFFLINE=true`.
3. Snapshots every file path and mtime under `target/**/.fingerprint/*/*`.
4. Sleeps two seconds, runs a second build, and snapshots again (Fresh/no-op
   comparison).
5. Touches `app/src/main.rs`, rebuilds, and snapshots (app-only rebuild with
   dependency reuse).
6. Builds and re-builds `--release` and `--target x86_64-unknown-linux-gnu`
   (only succeeds when the target is installed).
7. Copies the target directory to a seeded target directory and rebuilds there
   (mtime-preserving seed reuse model).
8. Emits `evidence.json` with versions, filesystem context, and a per-stage
   summary of whether any fingerprint file changed mtime.

The script refuses to run if the target directory is not empty and exits early
when the requested target triple is not installed.

## Expected result on Cargo 1.97.0

All no-op/reuse builds report `Fresh` and change **zero** fingerprint-file
mtimes. A real application rebuild changes only the application unit's
fingerprint files and the build-script *run* fingerprint; unrelated ordinary
and proc-macro fingerprints stay unchanged. Seeding a copy of the target
directory and building against it also changes no fingerprint mtimes.

## Safety conclusion

Newest mtime under `.fingerprint/<unit>` is **not** a last-use timestamp. It
is a compilation/materialization timestamp, with some action-sensitive
refreshes such as build-script run reevaluation. Consequently there is no safe
finite mtime cutoff that can distinguish "old and unused" from "old but
repeatedly reused." Fingerprint-derived deletion remains disabled/report-only
inside Djinn; whole-base eviction from epic `8s2o` is the sole destructive
bound.

See `server/docs/cargo-fingerprint-mtime-evidence.md` for the full
operator-facing evidence document.
