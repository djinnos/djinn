# Deferred merge-queue verdict grounding

**Status:** Deferred design note for proposal `xg69`, Phase 1 of epic `29ca`.

## Why Phase 1 cannot ground on this verdict

Phase 1 starts post-session knowledge extraction at the server-side task-run
completion trigger in `supervisor_runner`. At that moment the server has the
terminal `TaskRunReport` for the worker run and can truthfully pass its outcome,
park/failure reason, and any explicit review decision to extraction. The
merge-queue verdict is not part of that report: GitHub produces it later, after
the pull request enters the merge queue, and the PR poller observes the
`merge_group` result asynchronously.

Consequently, merge-queue verdict grounding is **explicitly deferred**, not
silently omitted. Phase 1 must not invent a merge-queue result from the task-run
outcome or conflate a later merge-queue rejection with a worker-run CI failure,
acceptance-criteria rejection, or review rejection.

## Phase 1 boundary

Phase 1 keeps post-session extraction fire-and-forget and best-effort. It does
**not** wait for merge-queue polling, and it does **not** implement either a
poller re-trigger or an annotation/re-extraction mechanism. Extraction failures
must not alter the terminal task-run outcome.

## Deferred follow-up options

A later phase may add one of these paths after the PR poller observes a durable
merge-queue verdict:

1. **Idempotent PR-poller re-trigger.** The PR poller may re-trigger extraction
   for the original task run after recording the merge verdict. The re-trigger
   must target the original task-run/session identity, rather than creating a
   synthetic run or treating the poller observation as a new worker attempt.
2. **Session-attributed annotation/re-extraction pass.** A later operation may
   attach the merge verdict as an annotation to the original extracted session
   and perform a guarded re-extraction or revision pass. The annotation and any
   resulting notes must retain attribution to that original task run and
   session.

## Required guardrails for either option

- **No live-trigger waiting:** the server-side completion trigger remains
  non-blocking; it never waits for a merge-queue poll or verdict.
- **No duplicate extraction:** use durable idempotency/deduplication keyed to
  the original task run and session, plus the relevant verdict/version where a
  revision is allowed. Repeated PR-poller observations must be harmless.
- **Preserve the task-run outcome:** a later verdict may add grounded context or
  a guarded revision, but must not rewrite, obscure, or cause extraction to
  change the original terminal task-run outcome.
- **Retain original attribution:** prompts, annotations, and any revised notes
  must identify the original task run and session; they must not attribute the
  merge-queue observation to a new poller run or unrelated session.

Until one of these mechanisms is designed and implemented with those guards,
Phase 1 extraction is grounded only in terminal facts available at its
server-side trigger.
