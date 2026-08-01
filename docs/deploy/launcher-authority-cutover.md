# Launcher authority administration

The one-shot cutover orchestrator is retired. Launcher authority remains a
durable, epoch-fenced database setting operated through `djinn-server admin`.
The read-only deploy preflight remains mandatory and does not change authority.

## Read the current mode and epoch

Run against the production database from an approved operator environment:

```bash
DJINN_DATABASE_URL=postgres://... \
  djinn-server admin launcher-authority show
```

Retain the exact output with the change record. It has this form:

```text
mode=resize-v2
epoch=4
```

An uninitialized or unavailable row is an error, never an implicit default.

## Change authority

Before changing mode, freeze catalog mutation and admission, run
`deploy/preflight/cutover-preflight.sh` against the exact chart and production
values, and independently verify zero live task-run Pods and zero nonterminal
resize or build-lease rows. Preserve all rollback image digests.

Use the epoch returned by `show`:

```bash
DJINN_DATABASE_URL=postgres://... \
  djinn-server admin launcher-authority set resize-v2 --expected-epoch 4
```

For rollback, repeat the same drain and preflight procedure, then target
`leaf-v1` with the current epoch:

```bash
DJINN_DATABASE_URL=postgres://... \
  djinn-server admin launcher-authority set leaf-v1 --expected-epoch 5
```

The command performs its own drain census and transactional fence. A non-empty
drain, stale epoch, unchanged mode, or unavailable row exits non-zero and does
not authorize admission to resume. Never retry with a guessed epoch: run
`show`, investigate the concurrent change, and restart the procedure.

After a successful set, run `show` again, confirm the requested mode and the
incremented epoch, deploy/re-enable admission, and retain the before/after
outputs with the preflight report and deployment revision.

## Refusal and recovery

- `drain refused`: keep admission paused; identify and settle the reported live
  Pods, permits, resize rows, or leases, then rerun preflight and the census.
- `epoch conflict`: another operator changed authority. Do not overwrite it;
  read the new state and reconcile the change record first.
- `uninitialized` or `unavailable`: stop. Repair the durable singleton through
  the approved database recovery process before attempting a mode change.
- post-change deployment failure: keep admission paused, verify retained
  `leaf-v1` artifacts are pullable, pass preflight for `leaf-v1`, drain again,
  and execute the epoch-fenced rollback command above.

The runtime reconciler and admin recovery surface are permanent. Do not restore
the retired `authority-cutover` binary or wrapper.
