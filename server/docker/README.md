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

## All-in-cluster smoke test (kind)

End-to-end walkthrough that stands up the whole djinn stack in a local
kind cluster and dispatches one real task-run Job that opens a GitHub PR.
Run from the repo root.

1. **Bring up kind + the local registry.**
   ```
   make kind-up
   ```
   Idempotent; safe to re-run. Leaves `kind-registry` (localhost:5001)
   running on the docker network.

2. **Build the images.**
   ```
   make image
   ```
   Builds `djinn-server:dev` and `djinn-agent-runtime:dev` from
   `server/docker/`.

3. **Push them into the kind-attached registry.**
   ```
   make image-push-local
   ```
   Retags as `localhost:5001/djinn-*:dev` and pushes; the kind node's
   containerd resolves `localhost:5001` through the registry container.

4. **Populate the GitHub App + vault key Secrets.**
   Create the namespace first so `kubectl create secret` has somewhere
   to land (`helm-install-local` would create it too, but the secrets
   need to exist before the Deployment rolls out or the server Pod will
   crashloop waiting for mounts).
   ```
   kubectl create namespace djinn --dry-run=client -o yaml | kubectl apply -f -

   # Vault key — 32 random bytes, base64-encoded is what djinn-db expects
   # at $DJINN_VAULT_KEY_PATH.
   openssl rand -out /tmp/djinn-vault.key 32
   kubectl create secret generic djinn-vault-key \
     --namespace djinn \
     --from-file=vault.key=/tmp/djinn-vault.key

   # GitHub App credentials. Replace <APP_ID>, paste the private-key PEM
   # downloaded from github.com/settings/apps/<your-app>/permissions.
   kubectl create secret generic djinn-github-app \
     --namespace djinn \
     --from-literal=app-id='<APP_ID>' \
     --from-literal=client-id='<CLIENT_ID>' \
     --from-literal=client-secret='<CLIENT_SECRET>' \
     --from-file=private-key.pem=/path/to/app-private-key.pem
   ```
   The chart reads these via `values.secrets.*.existingSecret`; the
   defaults in `values.local.yaml` already match the literal names above.

5. **Install the chart.**
   ```
   make helm-install-local
   ```
   Installs `djinn-crds` and `djinn` into the `djinn` namespace using
   `values.local.yaml` (hostPath PVCs, local-registry images).

6. **Port-forward the UI.**
   ```
   kubectl port-forward -n djinn svc/djinn-server 3000:3000
   ```

7. **Use the UI.** Open <http://localhost:3000>. Add an Anthropic (or
   your provider of choice) credential. Register the target GitHub
   project — the UI's "add project" flow clones the mirror into the
   `djinn-mirror` PVC under `/var/lib/djinn/mirrors/`.

8. **Dispatch a task.** In the UI, create a simple task like `"add a
   TODO line to README.md"`. Watch the Job lifecycle:
   ```
   kubectl get jobs -n djinn -w
   kubectl logs -n djinn -l app.kubernetes.io/component=taskrun -f --tail=-1
   ```

9. **Observe the PR.** The worker pushes its branch to the upstream
   repo and uses the GitHub App credentials to open a pull request on
   the target repo — check the repo's Pull Requests tab.

Teardown: `make helm-uninstall && make kind-down` (the local registry
container keeps its cached layers between cluster lifetimes, so the next
`make kind-up` is fast).
