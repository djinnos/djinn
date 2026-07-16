# JIT pitfall cohort effectiveness read

Use this operator artifact before creating any task that flips just-in-time (JIT) pitfall hints default-on. The goal is a real/cohort traffic read, not a unit-test proof. Local tests only prove the gate, telemetry labels, and transient response behavior; they do **not** prove production effectiveness.

Fill this document (or a copied dated instance) with evidence from a staging/cohort rollout window and leave the positive-read gate complete for the planner that will decide whether to create the default-on flip task. To reduce copy/paste drift, operators may start from the local template emitted by `./scripts/jit-pitfall-readout-bundle.sh`; the helper only emits placeholders/safe references and does not collect production telemetry.

## Scope and safety rules

- Feature under read: first `write`/`edit`/`apply_patch` in a session may append a transient `jit_pitfalls` response field containing a `<relevant-pitfalls>...</relevant-pitfalls>` block with the top scoped `pitfall`/`pattern` notes.
- Do not store prompt text, patch/source contents, or full rendered hint bodies in this artifact.
- Safe evidence: counts, rates, task/session/project identifiers already used operationally, rollout mode, bounded path summary, note ids/permalinks/types/ranks/confidence, and short operator classifications that do not reproduce hint body text.
- A positive read requires both effectiveness and noise checks. A telemetry-only read is insufficient.

## Rollout / kill-switch controls

### Session-start knowledge-context control

`DJINN_KNOWLEDGE_CONTEXT_ROLLOUT` controls the session-start
`knowledge_context` / `Relevant Knowledge` prompt block. It is an operator
safety control, not an effectiveness experiment: the knowledge-context kill
switch is **incident-response plumbing, not an experiment knob or randomized
control arm**. In particular, `cohort:<label>` is deployment metadata only;
it does not randomly assign sessions, create a per-session eligibility arm, or
change eligibility. Every otherwise eligible session injects in enabled and
cohort modes.

| Env/config | Effective label and behavior |
| --- | --- |
| `DJINN_KNOWLEDGE_CONTEXT_ROLLOUT` unset (and legacy value unset) | Default **enabled**. Eligible sessions load and render knowledge context. Trace label: `enabled`. |
| `DJINN_KNOWLEDGE_CONTEXT_ROLLOUT=enabled` | Eligible sessions load and render knowledge context. Trace label: `enabled`. |
| `DJINN_KNOWLEDGE_CONTEXT_ROLLOUT=cohort:<label>` | Eligible sessions load and render knowledge context for every eligible session. The full supplied value, including the label's case, is persisted verbatim (for example `cohort:Blue Canary`). It is not an assignment mechanism. Bare `cohort` and the compatibility aliases `staging`, `rollout`, and `controlled` persist `cohort`. |
| `DJINN_KNOWLEDGE_CONTEXT_ROLLOUT=off` | Suppresses the prompt block and records a durable `load_knowledge_context` trace with `rollout_label=off`, `trace_outcome=disabled_off`, no candidates, and zero estimated tokens. |
| `DJINN_KNOWLEDGE_CONTEXT_ROLLOUT=kill_switch` | Suppresses the prompt block and records a durable trace with `rollout_label=kill_switch`, `trace_outcome=disabled_kill_switch`, no candidates, and zero estimated tokens. `disabled`, `disable`, `killswitch`, `false`, and `0` are kill-switch aliases. |
| Legacy `DJINN_KNOWLEDGE_CONTEXT` with rollout unset | `1` is legacy-enabled and injects with `rollout_label=legacy_enabled`; every other legacy value is legacy-disabled. Legacy-disabled and unknown explicit rollout values suppress the block and record `rollout_label=legacy_disabled`, `trace_outcome=disabled_legacy`. An explicit rollout value takes precedence over the legacy value. |

The server prompt-rendering process owns `load_knowledge_context` rows and
writes them through its server database connection. The worker owns
`jit_pitfalls` rows and writes them through its worker database connection.
Neither writer needs a control-plane collector. Both writers are fail-open:
failed retrieval-trace persistence is logged as a warning and never blocks or
changes prompt assembly, a write, or an edit.

Treat a durable disabled row as evidence that the relevant process reached a
deliberate suppression decision. It is different from a missing/unrecorded
trace, which means there is no durable evidence of an attempt (for example,
the path was never reached or fail-open trace persistence failed). Do not
interpret missing rows as disabled traffic.

`memory_recall_trace` list and detail responses expose the trace-level
`rollout_label` and `trace_outcome` separately from per-candidate `outcome`.
Use `rollout_label` and `trace_outcome` list filters when triaging a rollout;
candidate filters remain for injected/skipped note candidates. This preserves
the compatibility API used by dispatch and other existing writers: they are
not relabeled by this knowledge-context/JIT work.

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

### Operator query cookbook templates

The examples in this section are **templates**. Adapt metric names, log table names, JSON extraction syntax, and task-outcome table names to the backend used for the rollout. They are provided only to help fill this artifact from real/cohort traffic; they do not claim that a cohort read has been performed or passed.

#### PromQL counter readout

Use the rollout start/end timestamps from the rollout record as `$window` (for example `6h`, `24h`, or the exact range selector supported by the metrics backend). The counter is intentionally labeled only by `outcome`, so use structured logs for project/cohort breakdowns.

```promql
# Counts by outcome over the rollout window.
sum by (outcome) (
  increase(djinn_jit_pitfall_hints_total[$window])
)

# Minimum table rows. Paste the resulting counts into the counter table below.
sum(increase(djinn_jit_pitfall_hints_total{outcome="eligible_search"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="injected"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="empty"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="error"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="disabled_default_off"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="disabled_kill_switch"}[$window]))
sum(increase(djinn_jit_pitfall_hints_total{outcome="non_first_modification"}[$window]))

# Derived rates for interpretation.
sum(increase(djinn_jit_pitfall_hints_total{outcome="injected"}[$window]))
/
sum(increase(djinn_jit_pitfall_hints_total{outcome="eligible_search"}[$window]))

sum(increase(djinn_jit_pitfall_hints_total{outcome=~"empty|error"}[$window]))
/
sum(increase(djinn_jit_pitfall_hints_total{outcome="eligible_search"}[$window]))
```

If the metrics backend supports dashboard variables, define `$outcome` as the seven stable outcome labels and use:

```promql
sum(increase(djinn_jit_pitfall_hints_total{outcome="$outcome"}[$window]))
```

#### Structured-log grouping templates

Use logs/traces with target `djinn_agent::jit_pitfalls` and message `jit_pitfalls telemetry outcome` plus the search-failed message for error paths. The examples below assume a normalized log table named `jit_pitfall_events`; replace it with the backend's log query syntax.

```sql
-- Outcome counts by rollout mode and project. Safe fields only.
SELECT
  rollout_mode,
  outcome,
  project_id,
  COUNT(*) AS event_count,
  COUNT(DISTINCT session_id) AS session_count
FROM jit_pitfall_events
WHERE ts >= :rollout_start
  AND ts < :rollout_end
  AND target = 'djinn_agent::jit_pitfalls'
  AND message IN (
    'jit_pitfalls telemetry outcome',
    'jit_pitfalls: scoped note search failed; skipping hint'
  )
GROUP BY rollout_mode, outcome, project_id
ORDER BY rollout_mode, outcome, event_count DESC;
```

```sql
-- Empty/error/non-first distribution by bounded path summary.
-- Do not join or select file contents; touched_path_summary is already bounded
-- as count=...;dirs=...;extensions=...
SELECT
  outcome,
  project_id,
  touched_path_summary,
  COUNT(*) AS event_count,
  COUNT(DISTINCT session_id) AS session_count,
  AVG(search_elapsed_ms) AS avg_search_elapsed_ms
FROM jit_pitfall_events
WHERE ts >= :rollout_start
  AND ts < :rollout_end
  AND outcome IN ('empty', 'error', 'non_first_modification')
GROUP BY outcome, project_id, touched_path_summary
ORDER BY event_count DESC
LIMIT 50;
```

```sql
-- Injected note distribution. Flatten the safe `notes` metadata array from
-- injected events; never select rendered hint body text.
WITH injected_notes AS (
  SELECT
    e.session_id,
    e.project_id,
    e.touched_path_summary,
    n.rank,
    n.id AS note_id,
    n.permalink,
    n.note_type,
    n.confidence
  FROM jit_pitfall_events e
  CROSS JOIN UNNEST(e.notes) AS n
  WHERE e.ts >= :rollout_start
    AND e.ts < :rollout_end
    AND e.outcome = 'injected'
)
SELECT
  note_type,
  rank,
  CASE
    WHEN confidence < 0.50 THEN '0.30-0.49'
    WHEN confidence < 0.75 THEN '0.50-0.74'
    ELSE '0.75+'
  END AS confidence_bucket,
  permalink,
  note_id,
  COUNT(*) AS injection_count,
  COUNT(DISTINCT session_id) AS session_count
FROM injected_notes
GROUP BY note_type, rank, confidence_bucket, permalink, note_id
ORDER BY injection_count DESC
LIMIT 100;
```

For log systems without SQL, apply the same grouping dimensions: `rollout_mode`, `outcome`, `project_id`, `session_id`, `touched_path_summary`, `search_elapsed_ms`, `result_count`, `rendered_note_count`, and injected-note metadata (`rank`, `id`, `permalink`, `note_type`, `confidence`).

#### Safe injected-vs-control outcome join

Join exposure events to task/session outcome fields using operational identifiers only. Do not collect prompt text, source text, patch text, rendered hint bodies, or full tool response JSON.

Template workflow:

1. Build an exposure set with one row per `session_id` in the rollout window:
   - `cohort = 'injected'` if any event for the session has `outcome = 'injected'`.
   - `cohort = 'eligible_empty'` if the session has `eligible_search` and `empty` but no `injected`.
   - `cohort = 'error_or_kill_switch'` if the session has `error` or `disabled_kill_switch`; exclude from the primary comparison or report separately.
   - `cohort = 'default_off_control'` for comparable sessions from a default-off/control window or deployment slice.
2. Map `session_id` to the task/session table already used operationally. Keep only identifiers and safe comparability fields: `task_id`, `session_id`, `project_id`, task type, priority, role/agent type, created/closed time, and rollout window label.
3. Join task outcome fields: `reopen_count`, `total_reopen_count`, `continuation_count`, `verification_failure_count`, and `total_verification_failure_count` (or documented equivalent fields if the backend names differ).
4. Aggregate by cohort and comparability dimensions. Inspect project/task-type/priority mix before interpreting outcome differences.

```sql
WITH exposure AS (
  SELECT
    session_id,
    ANY_VALUE(project_id) AS project_id,
    MAX(CASE WHEN outcome = 'injected' THEN 1 ELSE 0 END) AS had_injected,
    MAX(CASE WHEN outcome = 'eligible_search' THEN 1 ELSE 0 END) AS had_eligible,
    MAX(CASE WHEN outcome = 'empty' THEN 1 ELSE 0 END) AS had_empty,
    MAX(CASE WHEN outcome IN ('error', 'disabled_kill_switch') THEN 1 ELSE 0 END) AS had_error_or_kill_switch
  FROM jit_pitfall_events
  WHERE ts >= :rollout_start
    AND ts < :rollout_end
  GROUP BY session_id
), labeled_exposure AS (
  SELECT
    session_id,
    project_id,
    CASE
      WHEN had_injected = 1 THEN 'injected'
      WHEN had_eligible = 1 AND had_empty = 1 THEN 'eligible_empty'
      WHEN had_error_or_kill_switch = 1 THEN 'error_or_kill_switch'
      ELSE 'other'
    END AS cohort
  FROM exposure
), task_outcomes AS (
  SELECT
    s.session_id,
    s.task_id,
    t.project_id,
    t.issue_type,
    t.priority,
    t.agent_role,
    t.created_at,
    t.closed_at,
    t.reopen_count,
    t.total_reopen_count,
    t.continuation_count,
    t.verification_failure_count,
    t.total_verification_failure_count
  FROM operational_sessions s
  JOIN operational_tasks t ON t.task_id = s.task_id
)
SELECT
  e.cohort,
  COUNT(DISTINCT o.task_id) AS tasks,
  AVG(CASE WHEN o.reopen_count > 0 THEN 1.0 ELSE 0.0 END) AS reopen_rate,
  AVG(o.total_reopen_count) AS avg_total_reopens,
  AVG(CASE WHEN o.continuation_count > 0 THEN 1.0 ELSE 0.0 END) AS rework_continuation_rate,
  AVG(CASE WHEN o.verification_failure_count > 0 THEN 1.0 ELSE 0.0 END) AS verification_failure_rate,
  AVG(o.total_verification_failure_count) AS avg_total_verification_failures
FROM labeled_exposure e
JOIN task_outcomes o ON o.session_id = e.session_id
WHERE e.cohort IN ('injected', 'eligible_empty')
GROUP BY e.cohort
ORDER BY e.cohort;
```

For a default-off/control comparison, run the same task-outcome aggregation over the adjacent/control window and label those rows `default_off_control`; do not infer hint content from prompts or patches.

#### Noise-sampling pull template

Use a stratified or random sample of injected events for the false-positive/noise section. Select only the safe metadata needed to find the operational task/session and classify whether the note selection was useful.

```sql
WITH injected_notes AS (
  SELECT
    e.session_id,
    e.project_id,
    e.touched_path_summary,
    n.rank,
    n.id AS note_id,
    n.permalink,
    n.note_type,
    n.confidence,
    CASE
      WHEN n.confidence < 0.50 THEN '0.30-0.49'
      WHEN n.confidence < 0.75 THEN '0.50-0.74'
      ELSE '0.75+'
    END AS confidence_bucket
  FROM jit_pitfall_events e
  CROSS JOIN UNNEST(e.notes) AS n
  WHERE e.ts >= :rollout_start
    AND e.ts < :rollout_end
    AND e.outcome = 'injected'
)
SELECT
  session_id,
  project_id,
  touched_path_summary,
  note_id,
  permalink,
  note_type,
  rank,
  confidence,
  confidence_bucket
FROM injected_notes
-- Replace RANDOM() with backend-specific sampling, or stratify by project_id,
-- touched_path_summary, note_type, rank, and confidence_bucket.
ORDER BY RANDOM()
LIMIT :sample_size;
```

Operators should transcribe the sampled rows into the false-positive/noise table with a short classification (`useful`, `neutral`, `noisy`, or `false-positive`) and an action. The reviewer may inspect the operational task context as normally permitted, but this artifact must not store prompt text, patch/source content, rendered hint bodies, or note body excerpts.

#### Transcription example

The table below shows how to transcribe backend output into this artifact. Replace the example numbers with real rollout-window counts.

| Metric | Example backend output | Transcribe as |
| --- | ---: | --- |
| `eligible_search` count | 120 | Minimum counter table → Eligible search Count = `120`; denominator for injected/empty/error rates. |
| `injected` count | 72 | Minimum counter table → Injected Count = `72`; Rate = `72 / 120 = 60%`. |
| `empty` count | 44 | Minimum counter table → Empty/miss Count = `44`; Rate = `44 / 120 = 36.7%`. |
| `error` count | 4 | Minimum counter table → Error Count = `4`; Rate = `4 / 120 = 3.3%`; inspect error classes. |
| `non_first_modification` count | 310 | Minimum counter table → Non-first skipped Count = `310`; denominator should be all observed modifications. |
| `disabled_default_off` / `disabled_kill_switch` counts | 25 / 1 | Minimum counter table and empty/error/disabled read; confirm expected control traffic or deliberate kill-switch test. |

For the effectiveness table, paste only aggregate rows such as `Injected: tasks=68, reopen_rate=0.09, avg_total_reopens=0.12, rework_continuation_rate=0.16, verification_failure_rate=0.07`. Do not paste task prompts, patches, source snippets, or rendered hint text as supporting evidence.

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
