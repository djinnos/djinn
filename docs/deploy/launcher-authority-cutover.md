# Launcher authority cutover: `leaf-v1` → `resize-v2`, and back

Operator runbook for proposal `3i92`, epic `xowm`, task `eeky`.

Per-project images are built independently of the server and cannot be swapped
atomically by Helm. Moving the component that owns a build's CPU quota is
therefore not a deploy — it is an ordered sequence over a live catalog, and its
reverse has to be able to refuse itself.

> **This document is not the enforcement.** The ordering below is enforced by
> `ResizeRollout` in `server/src/task_run_resize_rollout.rs`: every step
> declares the steps that must already have run, the journal records only steps
> that actually completed, and calling the flip before the drain proof — or
> before the preflight — returns `StepOutOfOrder`. If this file and that module
> ever disagree, the module is right. What this file adds is the *operator*
> half — what you run, what you retain, and what to do when it blocks.

---

## What you run

```bash
# forward: leaf-v1 -> resize-v2
DJINN_CUTOVER_DIRECTION=activate \
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 \
DJINN_CUTOVER_PLAN=/path/to/plan.json \
DJINN_DATABASE_URL=postgres://... \
DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY=/path/to/legacy-digests.json \
DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY=<base64> \
DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE=<base64> \
  deploy/cutover/authority-cutover.sh deploy/helm/djinn --values prod-values.yaml

# reverse: resize-v2 -> leaf-v1
DJINN_CUTOVER_DIRECTION=rollback \
DJINN_CUTOVER_AUTHORITY_MODE=leaf-v1 \
  ... same variables ...
  deploy/cutover/authority-cutover.sh deploy/helm/djinn --values prod-values.yaml
```

**Exit status:** `0` the mode flipped and admission resumed; `1` blocked — the
mode did **not** move, and the driver says whether admission is left paused;
`2` unevaluable — a missing plan, an unreadable render, a probe image that is
not in the catalog. Nothing was attempted.

The wrapper delegates to `deploy/preflight/cutover-preflight.sh`, pointing
`CUTOVER_PREFLIGHT_BIN` at the `authority-cutover` binary. That is not a
shortcut: it is what makes the flip and the deploy gate render the same chart,
extract the same `DJINN_K8S_*` out of the same rendered `djinn-server`
container, and re-exec under the same `env -i`. A preflight verdict produced
under the operator's shell would not be a verdict about the deployment.

### The plan file

Everything one cutover run needs that a render cannot contain. Unknown keys are
rejected, an empty `retained` set is rejected (proving the pullability of
nothing is not evidence), and `role` is parsed, never defaulted.

```json
{
  "expected_epoch": 3,
  "registry_base_url": "https://ghcr.io",
  "reason": "3i92 launcher authority cutover",
  "probe_task_run_id": "019fc000-0000-7000-8000-000000000001",
  "probe_image_id": "i1",
  "retained": [
    {
      "image_id": "i1",
      "repository": "djinnos/djinn-image-i1",
      "digest": "sha256:<64 hex>",
      "role": "resize-v2-current"
    },
    {
      "image_id": "i1",
      "repository": "djinnos/djinn-image-i1",
      "digest": "sha256:<64 hex>",
      "role": "leaf-v1-rollback"
    }
  ]
}
```

`role` is one of `legacy-no-handshake`, `leaf-v1-rollback`,
`resize-v2-current`. `probe_image_id` must name a real, `ready`, selected
catalog row — the pause step proves itself by dispatching it, and a synthesised
image would prove the pause against a path no task run takes.

`expected_epoch` is read first (see "What you need before you start", item 3):
every flip is a compare-and-swap against it, so a second operator moving the
mode under you is a conflict, not a silent overwrite.

---

## The two authorities

| mode | who writes a build's CPU quota |
|---|---|
| `leaf-v1` | `djinn-cgroup-launcher` writes each invocation leaf's `cpu.max`. The launcher sidecar carries **no** container CPU limit — one there is an ancestor clamp over every leaf (task `7deu` measured a 4-core leaf burning 0.25). |
| `resize-v2` | Kubernetes in-place Pod resize owns the launcher sidecar's `limits.cpu`. The launcher must never write leaf `cpu.max`. |

There is no mode admitting both writers and none admitting neither. A running
Pod's authority is fixed when its image was built and its identity captured, so
flipping the mode under a live Pod does not migrate it — it strands it. That is
why every flip is drain-fenced.

The mode lives in one durable singleton row (`launcher_authority_mode`,
migration 167), seeded at `leaf-v1`. An **absent** row is not a default: every
caller fails closed.

---

## What you need before you start

1. **The signed legacy digest inventory.** Every dispatch-eligible image that
   declares no protocol must be listed, by immutable `sha256:` digest, in a
   document signed with the deployment's Ed25519 key.

   Build the candidate list from the catalog itself, never by hand:
   `ImageRepository::legacy_pre_protocol_digests()` returns exactly the `ready`,
   digest-pinned, no-handshake rows, ordered by id so the document is
   byte-reproducible and therefore its signature is.

   ```json
   {
     "schema_version": 1,
     "issuer": "platform-ops",
     "issued_at": "2026-07-31T00:00:00Z",
     "digests": ["sha256:<64 lowercase hex>", "..."]
   }
   ```

   Configure it:

   | variable | meaning |
   |---|---|
   | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY` | path to the document |
   | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_PUBLIC_KEY` | base64 raw Ed25519 public key (32 bytes) |
   | `DJINN_LEGACY_LAUNCHER_DIGEST_INVENTORY_SIGNATURE` | base64 Ed25519 signature (64 bytes) over the document's **exact bytes** |

   A mutable tag (`image:latest`) in `digests` is rejected at load. So is one
   flipped signature bit, one altered character of a listed digest, and a
   document signed by any key other than the configured one. An *unsigned*
   inventory is a legitimate dispatch-time state — it keeps an already-built
   catalog from being stranded by a missing config file — but it never
   authorizes a cutover.

2. **The retained set.** For each artifact you intend to be able to run
   afterwards — the `resize-v2` images you are activating, the `leaf-v1` images
   you would roll back to, and every allowlisted no-handshake digest — record
   the registry repository path and the immutable digest. Retention is proven by
   fetching the manifest and hashing the bytes; nothing in the check reads a
   stored column, so a `pullable` flag in your own tooling proves nothing here.

3. **The authority epoch.** Read it first; every flip is a compare-and-swap
   against it, so a second operator moving the mode under you is a conflict, not
   a silent overwrite.

---

## Forward activation

Run in this order. The driver refuses any other.

| # | step | what actually has to be true |
|---|---|---|
| 1 | **Freeze catalog mutation** | The old server is still live and still serving. Freezing first is what stops the inventory going stale between signing and use. |
| 2 | **Sign and load the legacy inventory** | The signature verifies over the document's exact bytes **and** the verified digest set covers every dispatch-eligible no-handshake row. A signed document that omits a live row would otherwise produce a green cutover and a refused dispatch afterwards. |
| 3 | **Deploy the protocol-aware server and the `pods/resize` RBAC** | Authority mode is **still `leaf-v1`**. The deploy is only safe because authority has not moved; finding `resize-v2` here means someone flipped ahead of the sequence. |
| 4 | **Rebuild and catalog images as `resize-v2`** | Every dispatch-eligible image declares `resize-v2` explicitly. Legacy and `leaf-v1` rollback digests are **retained**, not deleted. `resize-v2` images may be cataloged now; they do not dispatch while the mode is `leaf-v1`, and refusing them is the correct behaviour, not a defect. |
| 5 | **Verify retention** | Every retained digest is fetched from the registry and its content hashes to the recorded digest. |
| 6 | **Pause admission** | The pause is written **and then disbelieved**: a probe dispatch is issued through the same path a task run takes and must be refused. A pause row with no wired refusal blocks the cutover here rather than passing it. |
| 7 | **Prove the drain** | **Zero live task-run Pods** and **zero nonterminal resize/lease rows**. Both. See below. |
| 8 | **Clear the preflight** | `djinn_k8s::cutover_preflight::run` — the same validator `deploy/preflight/cutover-preflight.sh` runs, over the same render, assembled by the same module — must come back clean **for the mode being flipped to**, having actually evaluated its six classes. A clean verdict from zero evaluated classes blocks here. This step runs *after* the drain proof, not before, because the preflight judges the drain fence too and that fence is never empty on a live deployment. |
| 9 | **Flip the mode** | Compare-and-swap at the expected epoch, behind `set_mode`'s own transactional fence, which holds the `build_pod_permit_pools` row lock admission takes before it inserts. Requires steps 6, 7 **and** 8 in the journal; reaching it without any of them is `StepOutOfOrder`. |
| 10 | **Resume admission** | Only after a confirmed flip. Resuming earlier is `StepOutOfOrder`. |

### How step 4 is actually performed

The declaration an image carries is a **build input**, set on the deployment
that builds it:

```yaml
imagePipeline:
  controller:
    launcherAuthorityProtocol: "resize-v2"   # default "" → leaf-v1
```

rendered as `DJINN_IMAGE_LAUNCHER_AUTHORITY_PROTOCOL` on the server Pod and read
by `ImageControllerConfig::from_env`. Everything downstream follows from it: the
generated Dockerfile's `djinn.app/launcher-authority-protocol` LABEL, the env
the launcher sidecar reads out of the same image, and the sentinel the build Job
echoes into `images.launcher_authority_protocol`.

Two properties this step depends on, both enforced in code rather than by
procedure:

* **A protocol change cannot be served from cache.** The declaration is an input
  to `compute_environment_hash`, and the image tag is a prefix of that hash. A
  `leaf-v1` artifact and a `resize-v2` artifact of the same config therefore
  live at different tags, and flipping the value re-tags and rebuilds every
  catalog image rather than relabelling one whose launcher still writes leaf
  `cpu.max`. Budget for a full catalog rebuild here; the `leaf-v1` tags remain
  in the registry, which is what makes step 5's retention check — and rollback —
  possible.
* **The label and the catalog row cannot disagree.** `build_image_build_job`
  refuses to render a Job whose build context reports a protocol its own
  Dockerfile does not declare (`BuildContext::verify_declaration`), so a
  `leaf-v1` artifact catalogued as `resize-v2` is not a state the pipeline can
  reach.

Leaving the value unset keeps the built-in default (`leaf-v1`) and changes no
image hash, so a deployment that is not doing a cutover rebuilds nothing.

## Rollback

Same shape from step 5 onward, targeting `leaf-v1`, with the allowlist check
moved to the front — because under `leaf-v1` the no-handshake artifacts
dispatch, and they dispatch only because the signed inventory vouches for them.

1. Load the signed allowlist.
2. Verify retention of every `leaf-v1`/legacy digest.
3. Validate the catalog against `leaf-v1`.
4. Pause admission (proven by a refused dispatch).
5. Prove the drain.
6. Clear the preflight, **against `leaf-v1`**.
7. Flip to `leaf-v1`.
8. Resume admission.

**Rollback is blocked, and admission is left paused, when any of the first three
fail.** Concretely: the signed allowlist file is absent; a retained digest is no
longer pullable; a catalog row has been repointed to a digest the signed
document does not vouch for. In each case the mode does not move, admission is
not resumed, and no Pod is started. Do not "unblock" it by relaxing a check —
restore the artifact or re-sign the inventory.

"Admission is left paused" is reported from the **production dispatch-pause
predicate**, not from how far the run got. Steps 1-3 run before the rollback's
own pause step, so a rollback blocked there has an empty journal — and it will
still say `admission=PAUSED` if a half-finished forward cutover left a pause
behind, which is exactly when you need to know. An unreadable pause state is
reported as paused.

---

## The drain proof is two checks, and both are load-bearing

```
zero live task-run Pods   AND   zero nonterminal resize/lease rows
```

* **Nonterminal rows** come from `BuildPodPermitRepository::list_nonterminal_resize`,
  backed by `build_pod_permits_resize_nonterminal_idx` (migration 164). The six
  states are `birth_confirmed`, `lift_applying`, `lifted`, `drop_required`,
  `drop_applying`, `quarantined` — each means a Pod that may still be lifted or
  still owes a drop. The block names the **task-run ids and states**, not a
  count, because that is what you need at 03:00.
* **Live Pods** come from enumerating the apiserver. PostgreSQL cannot see a Pod
  whose permit was released, or that outlived its permit through a crash, and
  flipping under one strands it.

`set_mode` re-counts under its own lock and refuses again. That second fence is
not a substitute for the first: it reports counts, and it is blind to Pods.

### When the drain will not go empty

* A permit stuck in `drop_required`/`quarantined` is a Pod that owes a quota
  drop. Resolve the run; do not force the mode.
* A live Pod with no permit row is the crash-survivor case. Terminate it by
  UID-fenced delete and re-run the proof.
* An **unavailable** apiserver or an **unreadable** permit relation is never
  read as drained — both block, and both mean "come back when it answers".

---

## Preflight

The driver runs `djinn_k8s::cutover_preflight::run` itself, at step 8, and
refuses to flip when it blocks. Running it standalone first is still worth it:
it is the same verdict, minutes earlier, without pausing anything.

```bash
DJINN_CUTOVER_AUTHORITY_MODE=resize-v2 DJINN_DATABASE_URL=postgres://... \
  deploy/preflight/cutover-preflight.sh deploy/helm/djinn --values prod-values.yaml
```

Expect `drain-fence` to be the one class that blocks a standalone run against a
live deployment — the drain is not empty until admission has been paused, which
is why the flip's own preflight runs *after* the drain proof and not before.

```bash
# Retention, against a disposable registry:2 (isolated name and port, deleted
# on exit pass or fail).
bash deploy/preflight/tests/task-run-resize-rollout.sh

# The workflow itself, against real PostgreSQL.
cd server && cargo test -p djinn-server --test task_run_resize_rollout
# The operator entry point, end to end through ResizeRollout::production.
cd server && cargo test -p djinn-server --test authority_cutover
cd server && cargo test -p djinn-db  --test image_legacy_digest_allowlist

# The entry point is still reachable from a binary.
sh scripts/check-resize-reachability.sh
```

---

## What this cutover must never do

* Reintroduce a **blanket launcher CPU limit**. The ceiling render is
  `resize-v2`-only. Under `leaf-v1` a container limit on the launcher throttles
  every invocation leaf beneath it.
* Delete or disable `djinn-cgroup-launcher`, the process broker, the
  RuntimeClass/node assets, the credential-boundary tests, the broker mounts,
  `cgroup.kill`, or `wait_empty`. None of them is part of this transition.
* Grant compatibility to a **tag**. Only exact immutable digests are ever
  compared; `djinn-image-controller` has a fixture whose repository segment and
  tag suffix both read `resize-v2` for an artifact that declares `leaf-v1`.
