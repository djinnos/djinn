# CI Head Reconciliation — m116 Consumer Compatibility & ivek Boundary

> **Durable evidence** for epic `m116` (proposal `icoe`).  This document and
> the companion `types_tests.rs` regressions record that additive nullable CI
> head fields are safe for known consumers and that the scope boundary with
> proposal `ivek` is explicit.

## Consumer compatibility

### Fields added by m116

`CiGateSnapshot` (the CI payload returned by `task_show` and `task_list`)
now includes four additive nullable fields alongside the existing
backwards-compatible `head_sha`:

| Field                     | Type               | Serialization hint             |
|---------------------------|--------------------|--------------------------------|
| `mirror_head_sha`         | `Option<String>`   | absent from JSON when `None`   |
| `github_head_sha`         | `Option<String>`   | absent from JSON when `None`   |
| `heads_diverged`          | `Option<bool>`     | absent from JSON when `None`   |
| `head_observation_error`  | `Option<String>`   | absent from JSON when `None`   |

All four use `#[serde(skip_serializing_if = "Option::is_none")]`, so the
JSON wire shape when no reconciliation evidence is available is **identical**
to the pre-m116 payload.  Consumers that read only `head_sha` (or any other
existing field) never see the new keys unless upstream evidence populates
them.

### Known consumer surfaces

| Consumer surface                          | Impact of new fields                        |
|-------------------------------------------|---------------------------------------------|
| `task_show` / `task_list` JSON responses  | Additive; existing keys unchanged.           |
| MCP JSON schema (`CiGateSnapshot` $def)   | New optional properties; no required change. |
| Agent role tool schemas (architect, planner, worker, reviewer, lead, judge, adversary, advocate) | Schema snapshot already includes `CiGateSnapshot` via `task_show`; optional fields pass through. |
| `djinn_mcp_server.json` fixture           | Already synced with new fields.              |
| `mcp_tools_schema.snap` snapshot          | Already synced with new fields.              |
| Control-plane integration tests           | `task_tools.rs` snapshot test is additive.   |
| UI `CiGateSnapshot` type (`ui/src/api/types.ts`) | Interface does not declare the new fields; runtime ignores extra JSON keys. |
| UI `CiBadge` (`ui/src/components/TaskCard.tsx`) | Reads `status`, `gate_state`, `blocking_required_check_names`, etc.; never accesses reconciliation fields. |
| UI `CiStatusSection` (`ui/src/components/TaskDetailPanel.tsx`) | Reads `head_sha`, `status`, `gate_state`, `summary_reason`, etc.; never accesses reconciliation fields. |
| UI test fixtures (`ui/src/test/fixtures.ts`) | `mockCi*` objects omit the new fields; tests pass with or without them. |

#### UI forward-compatibility evidence

The UI deserializes the CI payload via standard `JSON.parse` (no strict
schema validation that would reject unknown keys).  The TypeScript
`CiGateSnapshot` interface (`ui/src/api/types.ts:24`) does **not** declare
`mirror_head_sha`, `github_head_sha`, `heads_diverged`, or
`head_observation_error`, so they are simply inaccessible properties at the
type level and inert extra keys at runtime.

The two rendering consumers read only pre-m116 fields:

- `TaskCard.tsx` (`CiBadge`): `ci.status`, `ci.gate_state`,
  `ci.blocking_required_check_names`, `ci.primary_blocking_check`,
  `ci.merge_blocked_reason`, `ci.summary_reason`.
- `TaskDetailPanel.tsx` (`CiStatusSection`): `ci.gate_state`, `ci.status`,
  `ci.summary_reason`, `ci.merge_blocked_reason`, `ci.head_sha`,
  `ci.pr_number`, `ci.blocking_required_check_names`,
  `ci.failure_fingerprint`, `ci.same_signature_count`.

Durable regression tests in `TaskCard.test.tsx` and
`TaskDetailPanel.test.tsx` simulate the real wire shape (a CI payload with
all four additive fields populated) and confirm that existing rendering
(`head_sha`, `status`, badge text) is unaffected.

#### Doctor and health surfaces

A targeted search for `CiGateSnapshot`, `ci_head`, or `head_sha` references
within `server/crates/` modules that import `doctor` or `health`
return **zero matches**.  While a broad `grep` for `doctor|health` in
`server/` does return many results (health-check endpoints, DB repository
modules like `doctor_finding.rs` and `board_health.rs`, and telemetry
labels), **none of these surfaces reference `CiGateSnapshot` or task CI
payload fields**.  The CI head reconciliation fields are confined to the
control-plane task-tool DTO layer and its consumers listed above.

### Consumer contract

- **Backward compatible:** existing `head_sha` consumers are unaffected.
  The `head_sha` field continues to represent the GitHub/PR CI head and
  serializes as a required string.
- **Forward compatible:** consumers that encounter unknown JSON keys
  (`mirror_head_sha`, `github_head_sha`, `heads_diverged`,
  `head_observation_error`) may safely ignore them.  Per JSON schema
  convention, additional optional properties are not errors.
- **Null semantics:** `heads_diverged` is `true` only when both heads are
  known and differ, `false` only when both are known and equal, and
  absent/null-compatible when either side is unknown.  Consumers should
  treat missing/null divergence as "unknown" (not "false").

## Proposal boundary: m116 vs `ivek`

### m116 owns (this epic)

- **Branch-publication/head-visibility mechanics:** exposing
  `mirror_head_sha`, `github_head_sha`, `heads_diverged`, and
  `head_observation_error` in task CI payloads so operators can observe
  mirror-vs-GitHub divergence.
- **Stale-head false-strike suppression for unpublished mirror commits:**
  when a WorkerDone mirror push succeeds but GitHub publication fails,
  unchanged-GitHub-head CI strikes/escalation are **not** counted for the
  unpublished mirror commit.  The coordinator surfaces
  divergence/publication-failure evidence instead.

### `ivek` owns (separate proposal)

- **Broader strike classification:** typed reopen classification,
  quality-strike guard aggregation, rework-loop park-guard semantics, and
  rejection-aware redispatch prompts.  These are general rework-loop
  mechanics, not specific to mirror/GitHub head divergence.
- **Submission-integrity fingerprints:** verifying that submitted code
  changes are real, that worktree edits were not lost, and that
  fabricated submissions are detected and escalated.  This is a
  submission-quality concern, not a head-visibility concern.

### Explicit non-overlap

m116 does **not** modify strike classification logic, reopen typing,
quality-strike guards, or submission-integrity checks.  Those are `ivek`
responsibilities.  Conversely, `ivek` does not add mirror/GitHub head
fields to CI payloads or suppress stale-head strikes for unpublished
commits — those are m116 responsibilities.

The proposals connect at one point: `ivek`'s broader strike classification
may eventually consume `heads_diverged` and `head_observation_error` as
signals, but `ivek` owns the decision logic for how those signals affect
strike counts, park guards, and escalation.

## Companion regression tests

### Control-plane (Rust)

`server/crates/djinn-control-plane/src/tools/task_tools/types_tests.rs`
contains the following CI head reconciliation regression tests (all
operate on local in-memory `Task` fixtures; no live external
infrastructure required):

- `ci_snapshot_without_reconciliation_fields_is_backwards_compatible`
  — no reconciliation fields → payload identical to pre-m116 shape.
- `ci_snapshot_equal_heads_serialize_diverged_false`
  — both heads known and equal → `heads_diverged: false`.
- `ci_snapshot_diverged_heads_serialize_diverged_true`
  — both heads known and differ → `heads_diverged: true`.
- `ci_snapshot_unknown_mirror_head_leaves_diverged_absent`
  — mirror head unknown → `heads_diverged` absent.
- `ci_snapshot_unknown_github_head_leaves_diverged_absent`
  — GitHub head unknown → `heads_diverged` absent.
- `ci_snapshot_head_observation_error_serializes_when_present`
  — observation error present → serializes as string.
- `ci_snapshot_reconciliation_fields_in_list_item`
  — list-item DTO also carries reconciliation fields.
- `forward_compatible_consumer_ignores_new_reconciliation_fields`
  — pre-m116 consumer struct deserializes a payload with all four new
    fields and `head_sha` / `status` remain intact.
- `forward_compatible_list_consumer_ignores_new_reconciliation_fields`
  — same check via the list-item DTO path.

### UI (vitest)

`ui/src/components/TaskCard.test.tsx` and
`ui/src/components/TaskDetailPanel.test.tsx` each contain a forward-
compatibility test that simulates the real wire shape (a CI payload with
all four additive m116 fields) and confirms existing rendering is
unaffected:

- `TaskCard`: renders `CI: passing` badge from a CI payload that also
  carries `mirror_head_sha`, `github_head_sha`, `heads_diverged`, and
  `head_observation_error`.
- `TaskDetailPanel`: renders `head_sha` (sliced to 8 chars), `Passing`
  status, and PR number from the same augmented payload.
