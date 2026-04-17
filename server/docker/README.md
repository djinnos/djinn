# `server/docker/`

Container assets for Djinn. The interesting one here is
`djinn-agent-runtime.Dockerfile` — the per-task-run sandbox image that
`LocalDockerRuntime` (see `server/crates/djinn-runtime/`) spawns one
container from per `TaskRunSpec`. See
`/home/fernando/.claude/plans/phase2-localdocker-scaffolding.md` for the
full design (§4 runtime, §5 image).

## Purpose

The runtime image is the "agent sandbox": a Debian-slim base with
`djinn-agent-worker`, rustup toolchain, node + tsserver/pyright, and
rust-analyzer. The server never builds code inside this image — it
pulls the prebuilt tag, then `docker run`s one container per task run,
piping a bincode `TaskRunSpec` in via stdin and opening a Unix-socket
RPC channel back to `SupervisorServices` on the host.

## Multi-stage layout

Three stages keep the published image small and reproducible:

1. **build** — `rust:1.82-slim-bookworm` + mold/clang; `cargo build
   --release -p djinn-agent-worker`; binary stripped.
2. **lsp** — throwaway layer pinning Node 20 LTS, typescript-language-server,
   pyright, and rust-analyzer. Cache-bustable without invalidating the
   runtime apt layers.
3. **runtime** — `debian:bookworm-slim` + rustup, python3, build-essential;
   worker binary + LSPs copied in; non-root user `djinn` (uid/gid 10001).

## Volume-mount contract

`LocalDockerRuntime` bind-mounts these host paths into every container:

| Host                                | Container         | Mode |
|-------------------------------------|-------------------|------|
| `<per-run workspace tempdir>`       | `/workspace`      | rw   |
| `$DJINN_HOME/mirrors`               | `/mirror`         | ro   |
| `$DJINN_HOME/cache/cargo`           | `/cache/cargo`    | rw   |
| `$DJINN_HOME/cache/pnpm`            | `/cache/pnpm`     | rw   |
| `$DJINN_HOME/cache/pip`             | `/cache/pip`      | rw   |
| `$DJINN_HOME/ipc`                   | `/var/run/djinn`  | rw   |

The worker runs as uid 10001, so the host paths above must be writable
by that uid (or owned by it). The top-level `docker-compose.yml` in the
repo root pre-creates these paths under `${DJINN_HOME:-~/.djinn}`.

## Env defaults baked into the image

```
CARGO_HOME=/cache/cargo
CARGO_TARGET_DIR=/workspace/target
PNPM_STORE_DIR=/cache/pnpm
PIP_CACHE_DIR=/cache/pip
RUSTUP_HOME=/usr/local/rustup
PATH=/usr/local/cargo/bin:/opt/node/bin:/usr/local/bin:/usr/bin:/bin
```

`LocalDockerRuntime` sets `DJINN_IPC_SOCKET=/var/run/djinn/<task-run>.sock`
and `RUST_LOG=...` per run.

## Build + tag

```
./server/docker/build-runtime-image.sh          # djinn-agent-runtime:dev
./server/docker/build-runtime-image.sh 0.1.0    # djinn-agent-runtime:0.1.0
```

The build context is the repo root (so the Dockerfile can `COPY server`
without pulling in `target/` — `.dockerignore` at the root keeps build
artifacts out).

## Why no compose service?

The image is started on-demand by `LocalDockerRuntime`, one container per
task run. Declaring it as a compose `service:` would spin up a dangling
idle container on `docker compose up`, which is not the intended
lifetime. Compose only owns the persistent server + data plane (dolt,
qdrant, djinn-server).
