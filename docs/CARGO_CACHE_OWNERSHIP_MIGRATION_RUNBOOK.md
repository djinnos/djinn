# Cargo cache ownership migration — one-time operator runbook

**Do not run any of this until BOTH of the following have merged and are running
in the cluster:**

| PR | What it changes |
|----|-----------------|
| #2658 | `cargo_target_seed` stops hardlinking build-script `OUT_DIR` into the warm base |
| this PR | the warm Job runs as uid `1001`; the task-run seed is performed through the broker at uid `1001` |

Running the `chown` before those land moves the base to an ownership no running
pod has, which is strictly worse than the mixed state it is in today. Running it
after is what makes both changes take effect on the *existing* 27 GiB base
instead of only on newly created files.

This is a **one-time** migration. Nothing in the codebase performs it, and
nothing should: a recursive `chown` over a multi-tens-of-GiB PVC is a
multi-minute stall, which is exactly why the Pod render pins
`fsGroupChangePolicy: OnRootMismatch` and why
`djinn_agent_worker::volume_contract` never repairs.

---

## Why

Two operation classes behave differently on a shared POSIX tree, and conflating
them is what produced the current state:

| Operation class | Examples | Governed by |
|-----------------|----------|-------------|
| directory-entry | `creat`, `unlink`, `rename`, `linkat` | the **directory's** mode |
| content | `open(O_TRUNC)`, `write`, `truncate` | the **file's** mode |
| **inode metadata** | **`chmod`, `chown`, `utimensat` with explicit times** | **ownership, and nothing else** |

The first two work cross-identity through gid `1000` + setgid + `g+w`, which is
what the existing volume-ownership contract establishes and what
`docs/VOLUME_OWNERSHIP_CONTRACT_RUNBOOK.md` documents.

The third does not, and cannot be made to. The kernel refuses `chmod` to a
non-owner with `EPERM` **even when the requested mode is byte-identical to the
current one**. No mode bit, no setgid bit, no POSIX ACL and no group membership
delegates it.

`std::fs::copy` always ends in `set_permissions`. So:

* a **build script** (uid `1001`, always — cargo runs as the launcher-spawned
  child) that copies its output over a seeded destination owned by uid `1000`
  writes the bytes and then fails at the final `chmod`;
* the **warm pod's** post-cycle `g+w` normalization pass
  (`volume_contract::normalize_warm_base_regular_files`) can only chmod files it
  owns.

The target state is therefore **two identities**, not one:

> Every actor that **creates content** in `/cache/cargo-target*` is uid `1001`.
> Actors that only manage **lifecycle** (create / delete / rename / readdir)
> stay where they are and work through gid `1000` + setgid + `g+w`.

| Actor | uid | Role after this migration |
|-------|-----|---------------------------|
| warm Job pod | `1001` | content creator |
| task-run cargo + build scripts | `1001` | content creator |
| task-run cargo-target **seed** | `1001` | content creator (brokered) |
| task-run worker process | `1000` | lifecycle only (creates/removes run dirs) |
| djinn-server / coordinator | `10001` | lifecycle only (`.warm-locks`, `.djinn-gc.lock`, prune, `read_dir`, `statvfs`) |

`djinn-server` deliberately does **not** move. It has no `chown` call site
anywhere in the Rust tree, and the only two `chmod` sites are in the worker.

---

## Measured starting state

Taken from the production k3s cluster (`kubectl -n djinn exec djinn-server-… -c
djinn-server`), project `019ea3bd-a305-73e3-806c-4edcc96ebfe2`, variant
`mold-jobs-4`:

```
base size                              27 GiB
total files                            16,138
debug/build                            2,993 files / 522 MiB
debug/.fingerprint                     7,270 files
uid histogram (maxdepth 3)             6,794 × 10001,  2,422 × 1000
zero-byte files under debug/build/*/out with nlink > 1        9
roots                                  drwxrwsr-x 10001:1000   (setgid + g+w already correct)
```

Two things to read off that:

* the group contract is **already satisfied** — this migration changes owners,
  not modes;
* the base is **mixed** `10001`/`1000`, not uniformly legacy. Neither identity
  can chmod the other's inodes, so the normalization pass has been partial for
  as long as both have existed.

The 9 zero-byte `nlink>1` files under `debug/build/*/out/` are the corrupted
units: a build-script `OUT_DIR` payload that was hardlinked into a private run
dir and then truncated *through the link* into the shared base. Cargo's
fingerprints still consider those units fresh, so nothing regenerates them.
#2658 stops new ones being created; it does not repair the existing nine.

---

## Procedure

### 0. Pre-flight

```bash
NS=djinn
kubectl -n "$NS" get pvc                       # confirm the real cache claim name
kubectl -n "$NS" get pods -l app.kubernetes.io/component=task-run
kubectl -n "$NS" get jobs -l app.kubernetes.io/component=graph-warm
```

Confirm the deployed worker image already contains both PRs. If the running warm
Job still renders `runAsUser: 1000`, stop — the image predates this change:

```bash
kubectl -n "$NS" get job -l app.kubernetes.io/component=graph-warm \
  -o jsonpath='{.items[*].spec.template.spec.securityContext.runAsUser}'
# expect: 1001
```

### 1. Quiesce

Nothing may be writing the base while it is chowned. A warm Job mid-compile
during the `chown` will emit `EPERM`s and can leave a half-written unit.

```bash
NS=djinn
# Stop the coordinator so it dispatches no new task-runs or warm Jobs.
kubectl -n "$NS" scale deploy/djinn --replicas=0

# Drain what is already running. Task-runs seed and compile into
# /cache/cargo-target-runs and read /cache/cargo-target.
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=graph-warm --ignore-not-found
kubectl -n "$NS" delete jobs -l app.kubernetes.io/component=task-run  --ignore-not-found

# Wait for the pods to actually go away, not just for the Jobs to be marked.
kubectl -n "$NS" wait --for=delete pod \
  -l app.kubernetes.io/component=task-run --timeout=300s || true
kubectl -n "$NS" get pods -l app.kubernetes.io/component=graph-warm
```

### 2. Delete the corrupted units

The nine zero-byte `OUT_DIR` payloads are unrecoverable, and their fingerprints
must go with them or cargo will keep treating the units as fresh.

**Recommended (simpler, and defensible):** wipe all of `debug/build` plus every
matching `debug/.fingerprint` entry. That is 2,993 files / 522 MiB out of a
27 GiB base — about 2% — and it costs only the build-script re-runs, not a
recompile of the workspace.

**Targeted alternative:** delete only the nine and their fingerprints. It saves
~500 MiB of rebuild and buys a much more delicate command; take it only if
build-script re-runs are known to be expensive for this project.

Both variants are inside the root pod below, which is the only context with
enough privilege. Pick one and delete the other before running.

### 3. The one-time chown, from a root pod

`kubectl run --overrides` is the shape this cluster already uses for volume
work (see `docs/VOLUME_OWNERSHIP_CONTRACT_RUNBOOK.md`). Substitute the real
claim name from step 0.

```bash
NS=djinn
CACHE_PVC=djinn-cache        # <-- from `kubectl -n $NS get pvc`

kubectl -n "$NS" run cargo-cache-ownership-fix --rm -i --restart=Never \
  --image=busybox:1.36 \
  --overrides='{
    "spec": {
      "securityContext": {"runAsUser": 0},
      "containers": [{
        "name": "fix",
        "image": "busybox:1.36",
        "command": ["sh", "-c", "set -eu\nB=/mnt/cache/cargo-target\n[ -d \"$B\" ] || { echo \"no cargo-target under $B\"; exit 1; }\n\necho \"== before ==\"\nfind \"$B\" -maxdepth 3 -printf \"%u\\n\" | sort | uniq -c\n\n# --- step 2: drop the corrupted build-script units -------------------\n# SIMPLE VARIANT (recommended): wipe debug/build + matching fingerprints.\nfor V in \"$B\"/*/mold-jobs-*; do\n  [ -d \"$V/debug/build\" ] || continue\n  echo \"pruning $V/debug/build\"\n  rm -rf \"$V/debug/build\"\n  rm -rf \"$V/debug/.fingerprint\"\ndone\n# TARGETED VARIANT (delete the block above and uncomment this instead):\n#for V in \"$B\"/*/mold-jobs-*; do\n#  find \"$V/debug/build\" -path \"*/out/*\" -type f -size 0 -links +1 2>/dev/null |\n#  while read -r F; do\n#    U=$(basename \"$(dirname \"$(dirname \"$F\")\")\")   # <pkg>-<hash>\n#    echo \"dropping unit $U (corrupt: $F)\"\n#    rm -rf \"$V/debug/build/$U\"\n#    rm -rf \"$V/debug/.fingerprint/${U%%-*}\"-*\n#  done\n#done\n\n# --- step 3: the one-time ownership move ----------------------------\necho \"chown -R 1001:1000 $B\"\nchown -R 1001:1000 \"$B\"\n\n# --- step 4: re-assert setgid on directories ------------------------\n# chown clears setuid/setgid on some filesystems; setgid is what makes new\n# files inherit gid 1000, so re-assert it unconditionally.\nchmod -R g+w \"$B\"\nfind \"$B\" -type d -exec chmod g+s {} +\n\necho \"== after ==\"\nfind \"$B\" -maxdepth 3 -printf \"%u %g %m\\n\" | sort | uniq -c\nls -land \"$B\"\necho done"],
        "volumeMounts": [{"name": "cache", "mountPath": "/mnt/cache"}]
      }],
      "volumes": [{"name": "cache", "persistentVolumeClaim": {"claimName": "djinn-cache"}}]
    }
  }'
```

Notes:

* `/cache/cargo-target-runs` is **not** touched. Run dirs are private,
  short-lived, and recreated per task-run; the quiesce in step 1 already
  removed the pods that owned them. If stale run dirs are present, delete them
  outright rather than chowning them.
* `chmod -R g+w` is safe here and is *not* the same as making files executable:
  it only adds the group-write bit the contract already requires.
* On a 27 GiB / 16k-file base this completes in well under a minute. It is the
  `chown` walk that costs, and 16k inodes is small.

### 4. Bring the cluster back

```bash
kubectl -n "$NS" scale deploy/djinn --replicas=1
kubectl -n "$NS" rollout status deploy/djinn --timeout=300s
```

---

## Do NOT do a cold re-warm instead

Deleting `/cache/cargo-target` entirely and letting the next warm Job rebuild it
is the cheap-looking option and it is a trap for this workspace.

`warm_job.rs` documents a cold first warm at **~20–25 minutes for a ~12-crate
workspace**, against `active_deadline_seconds` = `warm_job_timeout_seconds`
(default **3600 s**) with **`backoffLimit: 0`**. The djinn `server/` workspace is
~30 crates and its base measures 27 GiB. A cold warm that overruns the deadline
is SIGKILLed mid-compile, the Job is not retried, and the next warm tick starts
again from zero — an unbounded loop that never produces a base, while every
task-run compiles cold in the meantime.

The `chown` preserves the 27 GiB of already-compiled artifacts and costs under a
minute. Take it.

If a cold re-warm genuinely becomes unavoidable, raise
`DJINN_K8S_WARM_JOB_TIMEOUT_SECONDS` **first** — the `warm_job_timeout_seconds`
field doc in `djinn-k8s/src/config.rs` carries the full timing breakdown — and
never trim the compile set to fit the deadline, which silently reduces what the
base can seed.

---

## Verification

### Immediately after step 4

Ownership is uniform and the group contract is intact:

```bash
kubectl -n djinn exec deploy/djinn -c djinn-server -- sh -c '
  B=/var/lib/djinn/cache/cargo-target
  find "$B" -maxdepth 3 -printf "%u %g\n" | sort | uniq -c
  ls -land "$B"'
```

Expect a single `1001 1000` row (plus the roots) and `drwxrwsr-x` — the `s` is
the setgid bit; if it is missing, re-run the `find -type d -exec chmod g+s` from
step 3.

The worker's startup contract check must pass on the next pod. It fails closed,
so a violation is loud:

```bash
kubectl -n djinn logs -l app.kubernetes.io/component=task-run -c worker --tail=200 \
  | grep -E 'volume permission contract'
# expect: "volume permission contract satisfied"
# a violation logs "volume permission contract VIOLATED" with kind= and path=
```

### After the first warm Job completes

The normalization pass now owns everything it walks, so `files_unchangeable`
must be `0`:

```bash
kubectl -n djinn logs -l app.kubernetes.io/component=graph-warm --tail=-1 \
  | grep 'normalized group-write mode'
# expect: files_unchangeable=0
# a non-zero value means some inode is still owned by another uid — re-run step 3
```

### After the first task-run

The seed must report that it ran as the child, not the worker:

```bash
kubectl -n djinn logs -l app.kubernetes.io/component=task-run -c worker --tail=-1 \
  | grep -E 'cargo target seed'
# expect: "seeded at the cargo/build-script uid through the broker"  (seed_identity=child)
# a line carrying brokered_seed_degradation=<reason> means it fell back to the
# in-worker path — the run still succeeds, but the ownership fix is not in force
# for that run. The reason names which of no_broker / disabled_by_env /
# executable_unresolved / launch_failed / non_zero_exit / unparseable_summary.
```

### Within 24 h — the staleness alarm must clear

`djinn-coordinator`'s cargo cache health sweep warns once a base has gone
`WARM_BASE_STALE_AFTER_SECONDS` (24 h) without a rewrite. That is the only
signal for "warming stopped happening": seeding still reports `hit`, disk usage
is unchanged, and no warm Job fails.

```bash
kubectl -n djinn logs deploy/djinn --tail=-1 | grep 'cargo cache health'
# the INFO line carries warm_base_age_seconds — it must DROP after each warm Job
# the WARN "warm base has stopped re-converging" must stop appearing
```

If `warm_base_age_seconds` keeps climbing past 86,400 after the migration,
warming is not running at all and the problem is upstream of this runbook —
check that warm Jobs are being created and are not being 422'd at admission.

---

## Rollback

There is no partial rollback of the `chown` itself; it is idempotent and can be
re-run with a different target if needed. What can be rolled back independently:

* **the brokered seed** — set `DJINN_CARGO_TARGET_SEED_BROKERED=0` in the
  task-run worker environment. The seed reverts to the in-process uid-1000 path
  with no redeploy. Build scripts that copy over seeded entries can then fail at
  their final `chmod` again, which is the pre-change behaviour;
* **the warm pod uid** — reverting this PR's `warm_job.rs` change puts warm back
  at uid 1000. If the `chown` has already run, the warm pod can no longer chmod
  the base it inherits, so do this only together with a `chown -R 1000:1000`.

Reverting only #2658 while leaving this PR in place is **not** safe: the
hardlink-into-base behaviour it removes is what creates new corrupted units.
