# Shared-volume ownership contract — runbook

The `mirrors`, `cache`, and `projects` volumes are written by **three
identities**:

| Identity | Who | Where |
|----------|-----|-------|
| uid `10001` | `djinn-server` | `/var/lib/djinn/{mirrors,cache,projects}` |
| uid/gid `1000` | the task-run **worker process** | `/workspace`, `/cache`, `/mirror` |
| uid `1001`, primary group `1000` | the launcher-spawned child (compiles, tests, **and the cargo-target seed**) and the **warm Job pod** | `/workspace`, `/cache` |

> **The group carries directory-entry and content operations. It does not carry
> inode metadata.** `chmod`, `chown` and `utimensat` with explicit times are
> governed by **ownership alone** — `EPERM` to a non-owner even for a
> byte-identical mode, with no mode bit, setgid bit, ACL or group membership able
> to delegate them. Since `std::fs::copy` ends in `set_permissions`, every actor
> that **creates content** in `/cache/cargo-target*` must be the same uid, and
> that uid is `1001` because cargo and its build scripts are always the
> launcher-spawned child. That is why the warm Job runs as `1001` and why the
> task-run seed goes through the broker. Actors that only manage **lifecycle**
> (create / delete / rename / readdir) stay where they are. Moving an existing
> base to that state is a one-time operator action:
> `docs/CARGO_CACHE_OWNERSHIP_MIGRATION_RUNBOOK.md`.

They can only share those volumes under one contract:

* **group ownership = `1000`** (`ARTIFACT_GID`, `djinn_cgroup_launcher::child`)
  on every directory and file;
* **group-write (`g+w`)** on every directory — that is what lets any of the
  three create, replace, and unlink what another produced — and on every
  owner-writable file; for the Cargo warm base this explicitly **includes
  executables**. Owner-read-only files (`444` git loose objects and packfiles,
  cargo registry sources carrying the crate tarball's modes) remain exempt
  because they are replaced through the directory. Git template hooks outside
  that base retain their `755` exception. Warm-base executables must not be
  exempt: with `fs.protected_hardlinks=1`, a cross-uid task-run cannot hardlink
  a foreign-owned regular `755` file. Cargo
  can preserve that source mode when a build script copies an output, so the
  warm worker makes a full post-cycle pass over its Cargo base and adds `g+w`
  (`755` becomes `775`) to every regular file. This deliberately preserves the
  executable bit and does not recursively `chown`; the base has cross-identity
  writers, just like `/mirror`, so ownership alignment is not the mechanism;
* **setgid (`g+s`) on every directory** — so files created there *inherit* the
  artifact group instead of the creating process's primary group. On Linux a
  directory created inside a setgid directory inherits the bit as well, so
  normalizing the volume ROOT propagates it to everything created afterwards;
* **umask `0002`** in every process that writes these volumes, so new files land
  `664` and new directories `775`. The launcher-spawned child already set it;
  `djinn-server` (which clones the mirrors), the worker process, and the warm
  Job's clone wrapper now do too. Without it a
  process silently writes `755`/`644` into a perfectly conforming volume and
  breaks it from the inside — which is how the frozen warm base kept
  reappearing.

### Mirror ownership and Git trust

`/mirror` deliberately has a cross-identity owner contract: `djinn-server`
may create a mirror as uid `10001`, while task-run and warm workers consume it
as uid `1000`. Group `1000`, `g+w`, and setgid make the files mutually usable,
but Git's `safe.directory` ownership check requires a matching **uid**, not a
matching group. Therefore mirror ownership is not a readiness requirement and
operators must not recursively `chown /mirror` to uid `1000` merely to satisfy
Git.

Every Djinn-managed git process injects `safe.directory=*` through a **config
file** in protected (system) scope: `djinn_git::git_command` writes
`$TMPDIR/djinn-git-<euid>/gitconfig` once per process and exports
`GIT_CONFIG_SYSTEM` pointing at it. The warm Job's shell wrapper does the same
with its own file. This makes the 2026-07-25 operational
`chown /mirror/<project>.git` mitigation unnecessary for newly created,
restored, and freshly provisioned mirrors; retain the group, mode, setgid, and
umask contract above instead.

**Do not** switch this back to `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_0`/
`GIT_CONFIG_VALUE_0`, and not to `git -c` either. Both are *command* scope, and
`git clone --local` does ref discovery through an inner `git-upload-pack` child
that git spawns with those variables **stripped** — visible in `GIT_TRACE` as
`trace: run_command: unset GIT_CONFIG_COUNT GIT_DIR; git-upload-pack '<src>'`.
Measured on git 2.47.3 (the deployed server image) with uid 10001 cloning a
root-owned repository, `git clone --local --shared` fails with "detected dubious
ownership" under both command-scope forms and succeeds with `GIT_CONFIG_SYSTEM`.
The command-scope form is honoured by a git process that reads it *directly*,
which is why it looked correct, and it had in fact never worked for a mirror
clone in any released version — the check simply never fired while the server
owned the mirror.

`GIT_CONFIG_SYSTEM` rather than `GIT_CONFIG_GLOBAL`: `configure_private_dep_access`
stores the GitHub installation token as a `url.<...>.insteadOf` rewrite with
`git config --global`, and the agent's build tools (cargo, go, pnpm) read it back
from `$HOME/.gitconfig` with Djinn nowhere in the loop. Redirecting global scope
would send that write to a file nothing else reads and silently break
private-dependency fetches. System scope is additive, so `$HOME/.gitconfig` and
the XDG config keep being read exactly as before.

**Stopgap that is no longer needed.** On 2026-07-25, with v0.7.3 deployed,
`git config --global --add safe.directory "*"` was run by hand inside the running
server pod to unwedge PR creation (120 dubious-ownership errors and 40
`supervisor_pr_open failed` in 20 minutes). That was ephemeral — lost on every
pod restart, which is how the outage resurfaced after the rollout replaced the
pod — and it widened trust for every process sharing that uid, permanently, in a
file on disk. The generated file above replaces it: it is process-scoped, and it
is re-created on every start. Do not re-apply the manual command; if dubious
ownership reappears, the config file is missing or unwritable, and the server
logs `could not materialize the system-scope safe.directory config` at `WARN`.

Violating it does not produce a crash. It produces a **silent freeze**: the warm
Job's cargo phase is best-effort (lock-unavailable, step failure and timeout only
`WARN`) while the Job reports success from the graph phase, so a cache the warm
pod cannot write shows up as *green warm Jobs over a permanently frozen build
base*. This happened while preparing the v0.7.0 deploy — 13,512 files owned by
`10001` with dirs `755` / files `644` — and was caught by hand.

## What enforces it now

| Layer | Mechanism |
|-------|-----------|
| Fresh install | The chart's `fix-volume-perms` initContainer normalizes the three volume **roots** to `ownerUid:artifactGid` with `chmod 2775` (setgid + `g+w`). O(1), idempotent, **never recursive**. |
| Server access | The server pod gets `securityContext.supplementalGroups: [1000]` — membership in the artifact group with zero filesystem work. |
| Task-run pods | The Pod render pins `fsGroup: 1000` + `fsGroupChangePolicy: OnRootMismatch` and runs the worker container as uid/gid `1000`. The worker only manages lifecycle in the cargo trees; cargo, its build scripts and the cargo-target seed all run as the launcher-spawned child at uid `1001`. |
| Warm pods | Same `fsGroup` + `fsGroupChangePolicy`, but `runAsUser: 1001` (`CHILD_UID`) — the warm pod **creates content** in the shared cargo base, so it must be the same uid as the build scripts that later copy over it. A warm Job renders no launcher sidecar and no broker socket, so this does not touch the worker/child security boundary (asserted by `warm_pod_never_renders_a_launcher_sidecar`). |
| Runtime | `djinn-agent-worker` validates the **actually mounted** roots at startup (`volume_contract`) and fails readiness with a typed error naming the path and observed-versus-required ownership/mode. |

The startup check stats the workspace, the git metadata (`<workspace>/.git` and
the mirror root), and every configured cache root — `/cache`,
`/cache/cargo-target`, `/cache/cargo-target-runs`, `CARGO_HOME`, `SCCACHE_DIR`,
`CARGO_TARGET_DIR`, plus the warm Job's own Cargo warm-base variant. It also
asserts the process is a member of gid `1000`; a conforming volume is useless to
a pod that is not in the group.

It is **bounded**: depth 3, 32 entries per directory, 512 `lstat`s total. It
checks a *sample of the subtree*, not just the root, because the exact
production near-miss had a hand-fixed root over a broken subtree. A passing
sample (production has reported `entries_sampled=512, budget_exhausted=true`)
is not proof that every warm-base file conforms. The warm worker therefore does
a full regular-file mode normalization after each Cargo cycle; the startup check
**never repairs** — a recursive `chown` over a 300G cache is a multi-minute
stall on pod start, which is the whole reason `fsGroupChangePolicy` is
`OnRootMismatch`.

That normalization is **authoritative only over the files the warm pod owns**,
and saying otherwise was wrong for as long as the sentence existed. `chmod` is
granted by ownership, not by mode: a warm pod cannot add `g+w` to a file another
uid created, however conforming the directory is. A live production base
measured **6,794 files owned by uid `10001` against 2,422 owned by `1000`** —
neither identity could chmod the other's inodes, so the pass had always been
partial. It reports that honestly: `files_unchangeable` counts the refusals and
`first_chmod_error` names the first path, and a refusal does not abort the walk
(it used to, via `?`, which meant one foreign-owned file left every later file
unnormalized). `files_unchangeable > 0` after a warm cycle means
`docs/CARGO_CACHE_OWNERSHIP_MIGRATION_RUNBOOK.md` has not been run yet.

## `$HOME` is part of the startup check too (9jrg)

The volume contract covers the *mounted* surfaces. It did not cover the one
writable surface that ships inside the image: `$HOME` (`/home/djinn`).

`qut0` moved the Pod to uid/gid `1000` while the image still owned `/home/djinn`
as `10001:10001` mode `0775`. The group-write bit was there but pointed at a
group the Pod does not hold, so the Pod fell through to *other* (`r-x`) and could
not create a single entry under its own home. Nothing checked it. The first
consumer to notice was the durable output stash — `$HOME/.cache/djinn/output_stash`
when `XDG_CACHE_HOME` is unset — and it surfaced hours later, inside the reply
loop, as `create durable blobs: Permission denied (os error 13)` on **every**
worker and planner session. Because a worker dies before submitting for review,
the task returns to `open` and no reviewer is ever spawned; after six consecutive
failures the coordinator escalates to a 1800s cooldown, so planning looks dormant
rather than failing.

Two changes close it:

| Layer | Mechanism |
|-------|-----------|
| Image | `/home/djinn` keeps uid `10001` as its **owner** and is group-owned by the artifact GID `1000` with `2775`. Three identities write it — uid `10001` (server-side path), uid/gid `1000` (worker), uid `1001`/group `1000` (launcher-spawned child) — and only a shared group can hold all three. Changing the owner instead would break the first. Applied in `djinn-image-builder/scripts/base-debian.sh` (project images, `env-config/v11` forces the rebuild) and `server/docker/djinn-agent-runtime-base.Dockerfile`. |
| Job render | `XDG_CACHE_HOME=/cache/xdg/<project_id>` — the stash and the SCIP indexer cache resolve `$XDG_CACHE_HOME` first, so they no longer depend on the image home at all, and they land on a *persistent* PVC instead of a container layer that dies with the Pod. |
| Runtime | `volume_contract::check_home_writable` asks the kernel (`access(W_OK|X_OK)`) whether the running identity can create entries in `$HOME`, and fails readiness with the path, observed ownership/mode and running identity. |

The same rule applies to anything the image pre-creates *under* `$HOME` for the
runtime identity to write. `install-node.sh` scaffolds `$HOME/.local/state` so
`fnm` can drop its per-shell multishell symlink there; `mkdir` under the build
umask leaves it `0755`, which stopped working for the same reason the moment the
runtime identity stopped being the owner. It is now `2775` too. When adding a
build-time directory the Pod must write, group-own it to `1000` and give it
`2775` — owner-only modes are only safe for paths nothing writes at runtime.

`$HOME` is deliberately **not** a `VolumeRoot`: it is an image path, the
artifact-gid/setgid rules do not apply to it, and it is correct for it to stay
owned by uid `10001`. Only its effective writability is contractual.

A violation looks like:

```
volume permission contract VIOLATED  kind=home_not_writable path=/home/djinn ...
```

This fails closed. Task-run and warm Pods only ever start on a `ready` catalog
image, and the `env-config/v11` salt bump means every catalog image rebuilds
before it can be `ready` again — so a pre-fix image cannot be dispatched to. If
one somehow is, the break-glass is the existing, loud
`DJINN_VOLUME_CONTRACT_MODE=audit`; the correct fix is to let the image rebuild.

## Symptom → diagnosis

A pod that fails this check logs one line and exits:

```
volume permission contract VIOLATED  kind=group_write path=/cache/cargo-target/<project>/mold-jobs-8 ...
```

```bash
kubectl -n djinn logs job/<warm-job> | grep 'volume permission contract'
kubectl -n djinn get pods --field-selector=status.phase=Failed
```

Confirm from the volume side (`stat` on the offending path):

```bash
kubectl -n djinn exec deploy/djinn -- \
  stat -c '%n uid=%u gid=%g mode=%a' \
  /var/lib/djinn/cache /var/lib/djinn/cache/cargo-target
```

Conforming output is `gid=1000` and a mode beginning with `2` (setgid) and
carrying group-write: `2775` for directories, `664` for files.

## Before upgrading to the version that enforces this

The check fails closed. A cluster whose volumes predate the contract will see
its first post-upgrade task-run and warm Pods exit at startup with the log line
above **instead of** silently running against a frozen cache. Run the
remediation below in the same maintenance window as the upgrade — or run it
first, since it is safe on a conforming volume.

## One-time remediation for pre-existing volumes

Volumes provisioned before this contract (or restored from a backup taken before
it) carry the old ownership throughout their **contents**. The chart only
normalizes the roots, so those need a single operator pass. Run it once, with the
server scaled down and no task-run or warm Jobs in flight:

```bash
NS=djinn

# 1. Quiesce: stop new Jobs and drain the ones running.
kubectl -n "$NS" scale deploy/djinn --replicas=0
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=graph-warm --ignore-not-found
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=task-run  --ignore-not-found

# 2. One-shot root pod that mounts the same claims and normalizes them.
kubectl -n "$NS" run volume-ownership-fix --rm -i --restart=Never \
  --image=busybox:1.36 \
  --overrides='{"spec":{"securityContext":{"runAsUser":0},"containers":[{"name":"fix","image":"busybox:1.36","command":["sh","-c","set -eu; for d in /mnt/mirrors /mnt/cache /mnt/projects; do chgrp -R 1000 \"$d\"; chmod -R g+w \"$d\"; find \"$d\" -type d -exec chmod g+s {} +; done; echo done"],"volumeMounts":[{"name":"mirrors","mountPath":"/mnt/mirrors"},{"name":"cache","mountPath":"/mnt/cache"},{"name":"projects","mountPath":"/mnt/projects"}]}],"volumes":[{"name":"mirrors","persistentVolumeClaim":{"claimName":"djinn-mirrors"}},{"name":"cache","persistentVolumeClaim":{"claimName":"djinn-cache"}},{"name":"projects","persistentVolumeClaim":{"claimName":"djinn-projects"}}]}}'

# 3. Bring the server back; the startup check now passes.
kubectl -n "$NS" scale deploy/djinn --replicas=1
```

Substitute the real claim names (`kubectl -n "$NS" get pvc`) — they differ when
`storage.*.existingClaim` is set. On a large cache this takes minutes; that is
exactly why it is an operator action and not something a pod does at startup.

Cheaper-looking alternative when only the cache is broken: delete the Cargo warm
base (`/cache/cargo-target`, `/cache/cargo-target-runs`) and let the next warm
Job rebuild it under the correct ownership. It costs one cold warm cycle and no
`chown` walk.

**Do not take that option for a large Rust workspace.** `warm_job.rs` documents
a cold first warm at ~20–25 min for a ~12-crate workspace against a 3600 s
`active_deadline_seconds` with `backoffLimit: 0`; djinn's own `server/`
workspace is ~30 crates and its base measures 27 GiB. A cold warm that overruns
the deadline is SIGKILLed mid-compile, is not retried, and the next warm tick
starts from zero — an unbounded loop that never produces a base while every
task-run compiles cold. Prefer the targeted `chown` in
`docs/CARGO_CACHE_OWNERSHIP_MIGRATION_RUNBOOK.md`, which preserves the compiled
artifacts and completes in under a minute on 16k inodes.

### Break-glass

`DJINN_VOLUME_CONTRACT_MODE=audit` in the worker's environment downgrades the
failure to a loud `ERROR` log and lets the pod proceed over a known-broken
volume. It is a rollout-window escape hatch, **not** a fallback — nothing
reverts to the legacy uid, and the underlying breakage is still there. Outside
Kubernetes (no `KUBERNETES_SERVICE_HOST`) the check is `off`: there are no
pod-mounted volumes to validate.

## Why an explicit initContainer instead of relying on `fsGroup`

`fsGroup` + `fsGroupChangePolicy: OnRootMismatch` looks like it already
guarantees this. It does not, for four reasons:

1. **The trigger is a heuristic on the volume root.** The kubelet skips the
   recursive pass when the root's gid already matches the fsGroup *and* the root
   carries the expected setgid/permission bits. A root that was fixed by hand —
   precisely what happened here — makes the kubelet skip while the entire
   subtree stays wrong. The failure mode is exactly the silent one.
2. **When it does trigger, it is unbounded.** A root mismatch on a 300G cache
   means a full recursive `chown` at mount time, before any container starts:
   minutes of stalled pod start on every affected volume.
3. **It does not apply to every volume type.** `fsGroup` is a no-op for
   `hostPath` (what a single-node k3s install uses) and its behaviour varies
   across CSI drivers and `RunAsAny`-style policies.
4. **It couples membership to filesystem work.** All the server pod needs is to
   be *in* the group; `supplementalGroups` grants that with no ownership pass at
   all.

So the chart uses the explicit path: a root initContainer that normalizes only
the three volume **roots** (deterministic, O(1), driver-independent), plus
setgid so everything created below them inherits the group, plus
`supplementalGroups` for the server. `fsGroup: 1000` stays on the task-run and
warm Pod renders — with the roots already conforming, `OnRootMismatch` is a
guaranteed no-op there rather than a surprise recursive pass — and it is never
set to `Always`. The startup validator is the backstop that makes any remaining
gap loud instead of silent.

## Related

- `server/crates/djinn-agent-worker/src/volume_contract.rs` — the contract and
  the bounded check.
- `server/crates/djinn-agent-worker/tests/volume_contract.rs` — deterministic
  proof that each violation fails readiness and a conforming layout passes.
- `server/crates/djinn-agent-worker/tests/home_contract.rs` — the `$HOME` arm,
  proved against a directory owned by a **different** uid than the running
  process (privileged runs fabricate `10001:10001 0775` and `10001:1000 2775`
  and judge both from a child dropped to uid/gid 1000).
- `deploy/helm/djinn/tests/volume-ownership-render.sh` — the render contract.
- `server/crates/djinn-k8s/src/launcher.rs` — `pod_security_context()` and the
  worker/child/launcher uid contract.
- `server/crates/djinn-git/src/lib.rs` — the generated protected-scope
  `safe.directory` config, with the measurements behind it.
- `server/crates/djinn-git/src/lib_tests.rs` — regression tests that assert which
  scope git resolves the rule in and what the inner child of
  `git clone --local` sees. "A clone of a foreign-owned repo succeeds" is not
  sufficient: git >= 2.48 accepts it either way, so that assertion passes on a
  modern developer/CI git while production is wedged.
