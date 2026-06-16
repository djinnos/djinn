# JIT pitfall cohort effectiveness read

Use this operator artifact before creating any task that flips just-in-time (JIT) pitfall hints default-on. The goal is a real/cohort traffic read, not a unit-test proof. Local tests only prove the gate, telemetry labels, and transient response behavior; they do **not** prove production effectiveness.

Fill this document (or a copied dated instance) with evidence from a staging/cohort rollout window and leave the positive-read gate complete for the planner that will decide whether to create the default-on flip task. To reduce copy/paste drift, operators may start from the local template emitted by `./scripts/jit-pitfall-readout-bundle.sh`; the helper only emits placeholders/safe references and does not collect production telemetry.

## Scope and safety rules

- Feature under read: first `write`/`edit`/`apply_patch` in a session may append a transient `jit_pitfalls` response field containing a `<relevant-pitfalls>...</relevant-pitfalls>` block with the top scoped `pitfall`/`pattern` notes.
- Do not store prompt text, patch/source contents, or full rendered hint bodies in this artifact.
- Safe evidence: counts, rates, task/session/project identifiers already used operationally, rollout mode, bounded path summary, note ids/permalinks/types/ranks/confidence, and short operator classifications that do not reproduce hint body text.
- A positive read requires both effectiveness and noise checks. A telemetry-only read is insufficient.

## Rollout / kill-switch controls

Primary control surface implemented by the JIT handler:

| Env/config | Values | Expected telemetry / behavior |
| --- | --- | --- |
| `DJINN_JIT_PITFALLS_ROLLOUT` unset or `off` | Default pre-read state | No search, no `jit_pitfalls` response field, counter outcome `disabled_default_off`, tracing `rollout_mode="default_off"`. |
| `DJINN_JIT_PITFALLS_ROLLOUT=enabled` (also `enable`, `on`, `true`, `1`) | Explicit operator opt-in | Eligible first modifications search and may inject hints. Tracing `rollout_mode="enabled"`. |
| `DJINN_JIT_PITFALLS_ROLLOUT=cohort` (also `staging`, `rollout`, `controlled`) | Controlled cohort/staging traffic | Eligible first modifications search and may inject hints. Tracing `rollout_mode="cohort"`. Use this for the effectiveness read unless a narrower deployment mechanism is already selecting the cohort. |
| `DJINN_JIT_PITFALLS_ROLLOUT=disabled` (also `disable`, `kill_switch`, `killswitch`, `false`, `0`) | Explicit disable / kill switch | Overrides legacy opt-in. No search, no `jit_pitfalls` response field, counter outcome `disabled_kill_switch`, tracing `rollout_mode="kill_switch"`. |
| `DJINN_JIT_PITFALLS=1` with `DJINN_JIT_PITFALLS_ROLLOUT` unset | Legacy migration opt-in only | Eligible first modifications search and may inject hints. Tracing `rollout_mode="legacy_opt_in"`. Do not use as the primary rollout control for this read. |

Rollout record:

- Environment / cluster:
- Cohort selection rule (projects, users, role, or staging namespace):
- Rollout start / end UTC:
- Config applied:
- Kill-switch command/runbook link or exact revert:
- Operator who can execute kill switch:
- Evidence that kill switch was tested or is already known safe:

## Telemetry to collect

### Counter

Collect the Prometheus counter `djinn_jit_pitfall_hints_total{outcome="..."}` for the rollout window and an adjacent/control window where useful.

Stable outcome labels:

| Outcome label | Meaning |
| --- | --- |
| `disabled_default_off` | Handler observed a modification while default-off/off and did not search. |
| `disabled_kill_switch` | Handler observed a modification while explicit kill switch disabled the feature and did not search. |
| `non_first_modification` | Session already consumed its one JIT opportunity; later modification skipped. |
| `eligible_search` | First eligible modification passed gate and attempted scoped note search. |
| `injected` | One or more notes rendered into transient `jit_pitfalls`. |
| `empty` | Search succeeded but found no matching scoped notes. |
| `error` | Missing project/path input or scoped search error; write/edit still succeeded without a hint. |

Minimum counter table:

| Metric | Count | Rate / denominator | Notes |
| --- | ---: | ---: | --- |
| Eligible search (`eligible_search`) |  | eligible / first modifications |  |
| Injected (`injected`) |  | injected / eligible |  |
| Empty/miss (`empty`) |  | empty / eligible |  |
| Error (`error`) |  | error / eligible |  |
| Disabled default-off (`disabled_default_off`) |  | disabled / modifications |  |
| Disabled kill-switch (`disabled_kill_switch`) |  | kill-switch / modifications |  |
| Non-first skipped (`non_first_modification`) |  | non-first / modifications |  |

### Structured tracing fields

Collect structured events with target `djinn_agent::jit_pitfalls` and message `jit_pitfalls telemetry outcome` (or the search-failed message for errors). Use these fields:

- `outcome`
- `rollout_mode`
- `session_id`
- `project_id`
- `touched_path_count`
- `touched_path_summary` (`count=...;dirs=...;extensions=...`; bounded summary, not source contents)
- `search_elapsed_ms` when a search was attempted
- `result_count` when a search completed or errored
- `rendered_note_count`
- `notes` for injected events only, containing safe note metadata: `rank`, `id`, `permalink`, `note_type`, `confidence`
- `error` for error paths

Do **not** collect or paste the transient `jit_pitfalls` field value, full `<relevant-pitfalls>` body, prompt text, patch text, or source file contents.

### Note distribution read

For injected events, summarize only safe metadata:

| Dimension | Summary |
| --- | --- |
| Note types (`pitfall`, `pattern`) |  |
| Rank distribution (1/2) |  |
| Confidence buckets (for example 0.30-0.49, 0.50-0.74, 0.75+) |  |
| Top safe note permalinks/ids by injection count |  |
| Projects/path-summary buckets with high empty rate |  |

## Outcome effectiveness read

Compare injected task traffic against comparable non-injected traffic. Prefer a cohort/control split during the same time window; if not possible, use adjacent windows with the same project mix and task types.

Recommended unit of analysis:

- Exposure: a task/session with at least one `injected` JIT event.
- Control: comparable task/session with `eligible_search` but `empty`, or same cohort/time window where rollout was default-off and no injection occurred.
- Exclude or separately bucket sessions affected by `error` or `disabled_kill_switch`.

Outcome fields to join from task data / activity:

- `reopen_count` and `total_reopen_count`
- `continuation_count` as a rework/extra-session proxy
- `verification_failure_count` and `total_verification_failure_count`
- Review rejection / verification-failure activity when available
- Task type, priority, project, role/agent type, created/closed time, and cohort window for comparability

Effectiveness table:

| Cohort | Tasks/sessions | Reopen rate | Avg total reopens | Rework/continuation rate | Verification-failure rate | Avg total verification failures | Notes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| Injected |  |  |  |  |  |  |  |
| Eligible but empty |  |  |  |  |  |  |  |
| Default-off/control window |  |  |  |  |  |  |  |

Interpretation checklist:

- [ ] Injected traffic is not obviously easier/harder than control traffic (project/task type/priority mix checked).
- [ ] Reopen/rework/verification-failure rates for injected traffic are lower than or not worse than comparable non-injected traffic.
- [ ] Any apparent improvement is not explained solely by low sample size or one unusually easy project.
- [ ] Empty/error/disabled rates are understood and do not indicate the feature is usually unavailable.
- [ ] No production incident, latency spike, or operator complaint is attributed to the rollout.

## Empty/error/disabled read

| Check | Result | Follow-up needed? |
| --- | --- | --- |
| Empty rate acceptable or explained by missing scoped notes? |  |  |
| Error rate near zero? If not, top error classes from `error` field? |  |  |
| Kill-switch outcome absent except deliberate tests/incidents? |  |  |
| Default-off outcomes only from non-cohort/control traffic? |  |  |
| Non-first skipped count consistent with once-per-session design? |  |  |

## False-positive / noise sampling

Sample injected events without storing prompt text, patch contents, source contents, or the rendered hint body.

Sampling plan:

- Sample size:
- Selection method (random, stratified by project/path summary/note type/confidence bucket):
- Reviewer/operator:
- Date:

Per-sample fields allowed:

| Sample id | Task/session id | Project | Path summary | Note safe metadata (id/permalink/type/rank/confidence) | Operator classification | Action |
| --- | --- | --- | --- | --- | --- | --- |
|  |  |  |  |  | useful / neutral / noisy / false-positive | keep / edit note metadata / archive note / tune scope |

Rules:

- Do not paste the hint body or any prompt section.
- Do not paste patch/source content used by the agent.
- Classification should be a short judgment, e.g. "noisy because note scope is too broad for Rust tests", not a reproduction of note content.
- If a note is repeatedly noisy, link the safe note permalink/id and create a separate note-maintenance task.

Noise summary:

| Classification | Count | Rate | Notes / follow-up |
| --- | ---: | ---: | --- |
| Useful |  |  |  |
| Neutral |  |  |  |
| Noisy |  |  |  |
| False-positive |  |  |  |

## Prompt-budget read

JIT hints are intentionally transient response fields. They must not increase the session-start `Relevant Knowledge` block.

Confirm:

- [ ] Session-start note injection remains the existing `knowledge_context` / `Relevant Knowledge` prompt section and is unchanged or reduced during the rollout.
- [ ] JIT hints appear only on the first modification tool result as the transient JSON field `jit_pitfalls`.
- [ ] The rendered hint is not stored in telemetry, durable memory, task comments, activity logs, or this readout.
- [ ] Later write/edit/apply_patch responses in the same session do not append another hint (`non_first_modification` outcome accounts for skips).
- [ ] No code path has moved the JIT block into system/developer prompts or session-start context.

Evidence:

| Evidence item | Result |
| --- | --- |
| Session-start prompt/token sample before rollout |  |
| Session-start prompt/token sample during rollout |  |
| Difference (unchanged/reduced required) |  |
| Tool response sample confirms transient `jit_pitfalls` only |  |
| Telemetry/log sample confirms no hint body text persisted |  |

## Positive-read summary / default-on gate

A later planner must be able to inspect only this section and decide whether creating a default-on flip task is justified.

**Gate status:** `UNKNOWN`

Operator instructions:

- Leave the gate status as `UNKNOWN` until a real staging/cohort read has completed and the required evidence rows below are filled in.
- After the read, edit the single gate status value to exactly `PASS` or `FAIL`; do not add slash-separated alternatives or extra current statuses.
- A later planner may create the default-on flip task only when all of these are true:
  - **Gate status** is exactly `PASS`.
  - Every required evidence row below is completed with a `PASS`/`DONE` status and a link or short summary.
  - `Recommendation:` is exactly `create default-on flip task`.
- `UNKNOWN` or `FAIL` must not produce a default-on flip task; instead leave the feature default-off and create follow-up work only if the operator recommendation asks for it.

| Required evidence | Status | Link / short summary |
| --- | --- | --- |
| Controlled rollout/cohort was enabled with documented kill switch |  |  |
| Telemetry counts collected for eligible, injected, empty, error, disabled, and non-first skipped outcomes |  |  |
| Injected vs non-injected outcome comparison completed using reopen/rework/verification-failure measures |  |  |
| Empty/error/disabled rates acceptable or follow-up tasks created |  |  |
| False-positive/noise sampling completed without storing prompt/patch/hint body text |  |  |
| Prompt-budget check confirms session-start note injection unchanged or reduced and JIT hints remain transient response fields |  |  |
| Operator recommendation recorded |  |  |

Decision:

- Recommendation: `extend cohort`
- Rationale:
- Required follow-up before default-on:
- Operator/date:
