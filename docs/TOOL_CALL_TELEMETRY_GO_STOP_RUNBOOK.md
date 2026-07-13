# Tool-call telemetry GO/STOP analysis runbook

This runbook defines the operator-repeatable repository workflow for the Phase 1
Codex-shaped tool-surface decision gate (epic `0tyy`, proposal `a5ht`). It uses
`djinn_db::repositories::tool_call_export` and
`djinn_db::repositories::tool_call_evaluator` to normalize persisted session
transcripts, derive metrics, and evaluate a fixed-window GO/STOP report.

**Important:** This runbook documents the workflow and input contracts. The
synthetic fixtures in this repository run in CI to prove the pipeline is
deterministic. Production transcript collection, credential access, Langfuse/OTel
enrichment, and the 20-trace manual audit are **operator inputs** performed
outside of CI. CI does not assert that production collection or manual review
occurred.

## What this workflow produces

A single GO/STOP/insufficient-data decision record for a completed 30-day window
with:

- Window metadata (start, end, source description)
- Source completeness and missing-field diagnostics
- Candidate population (Codex / OpenAI Responses surface) and matched baseline
  population (default / Responses/default surface) sample sizes and rates
- All sample minima, pseudo-count ratio, and Wilson difference interval
- Each threshold outcome
- 20-trace manual audit result
- Final decision and rationale

## Prerequisites

- Access to the persisted `sessions` and `session_messages` tables that contain
the target 30-day window.
- Optional: Langfuse trace IDs or OTel span IDs correlated with the session/task
IDs. These are **trace enrichment**, not the primary transcript authority.
- Optional: `provider_id`, `format_family`, and `tool_surface_family` values from
provider/catalog resolution when the persisted session row does not already
carry them.

## 1. Select and record the 30-day window

Choose an inclusive 30-day calendar window. The evaluator requires exactly 29 days
between `start_day` and `end_day` (inclusive):

```text
start_day = 2026-07-01
end_day   = 2026-07-30
```

Record the window in the decision record:

```yaml
window:
  start_day: "2026-07-01"
  end_day: "2026-07-30"
  source_description: "persisted session transcripts, production DB read-only replica"
```

## 2. Export persisted transcripts

The exporter is the **authority** for tool-call identity, turn order, result
status, and read-truncation detection. Optional Langfuse/OTel data only enriches
rows that already exist in the transcript.

### Input contract: `PersistedTranscript`

The exporter constructs this internally from a persisted session record and its
ordered `session_messages` rows. The operator does not need to build the struct by
hand; the repository API performs the normalization. The shape is documented for
traceability:

```rust
use djinn_db::{
    ExportDimensions, PersistedTranscript, ToolCallExportRepository,
    normalize_persisted_transcript,
};
use djinn_core::models::{SessionRecord, SessionMessage};

// Loaded from the production read replica by the repository:
let session: SessionRecord = /* SELECT * FROM sessions WHERE id = ... */;
let messages: Vec<SessionMessage> = /* SELECT * FROM session_messages
                                      WHERE session_id = ...
                                      ORDER BY created_at ASC, id ASC */;

let dimensions = ExportDimensions {
    provider_id: Some("openai".into()),
    format_family: Some("OpenAIResponses".into()),
    tool_surface_family: Some("codex".into()),
};

let rows = normalize_persisted_transcript(
    &PersistedTranscript { session, messages, dimensions },
);
```

### Repository export (single session)

```rust
let repo = ToolCallExportRepository::new(db);
let rows = repo.export_session(session, dimensions).await?;
```

### Executable full-window export and report

The checked-in example `server/crates/djinn-db/examples/run_telemetry_analysis.rs`
performs the entire window selection, export, optional enrichment, matched-baseline
construction, report evaluation, and deterministic 20-trace failed-edit audit
frame in one command. It is a production operator tool and is **not** run by CI.

```bash
cd server
DATABASE_URL=postgres://... cargo run -p djinn-db --example run_telemetry_analysis -- \
  --window-start 2026-07-01 \
  --window-end 2026-07-30 \
  --output report.json \
  --candidate-family codex \
  --baseline-families default,Responses/default
```

The command will:

1. Query the `sessions` table for rows whose `started_at` falls in the inclusive
   30-day window and whose `task_id` is not null and `agent_type` is not `chat`.
2. Export each session with the candidate dimensions (`OpenAIResponses`/`codex`).
3. Export each session again with the baseline dimensions (`default`,
   `Responses/default`) to build the matched baseline pool.
4. Join the optional Langfuse/OTel enrichment step (inserted between export and
   evaluation) if the operator has it.
5. Run `evaluate(...)` with the default `SampleMinima` and `GateThresholds`.
6. Write the JSON report to `--output`.
7. Print the deterministic 20-trace failed-edit audit frame to `stderr`.

With no `--audit-*` flags the report is intentionally `insufficient data` because
a manual audit has not yet been supplied. After classifying the sample, re-run
with the audit counts:

```bash
DATABASE_URL=postgres://... cargo run -p djinn-db --example run_telemetry_analysis -- \
  --window-start 2026-07-01 \
  --window-end 2026-07-30 \
  --output report.json \
  --candidate-family codex \
  --baseline-families default,Responses/default \
  --audit-sampled 20 \
  --audit-qualifying 12
```

For a full window, collect sessions where `started_at` falls inside the window,
then call `export_session` for each and concatenate the resulting
`Vec<NormalizedToolCallRow>`.

### Output contract: `NormalizedToolCallRow`

Each row carries the required dimensions:

- `provider_id`
- `model_id`
- `format_family`
- `tool_surface_family`
- `agent_role`
- `session_id`
- `task_id`
- `calendar_day`
- `tool_call_id`, `turn_index`, `tool_name`, `args_hash`
- `result_status`, `error_class`, `error_text`, `read_truncated`
- `path`, `read_offset`, `read_limit` (for path/loop metrics)
- `diagnostics` (missing-field markers)

## 3. Optional trace enrichment (Langfuse/OTel)

Trace enrichment is optional and must never replace a transcript row. Join on
`session_id` and/or `task_id` and copy only these bounded fields into the
normalized row or into a sidecar enrichment map:

- Provider latency / token counts
- Trace ID for later audit sampling
- Resolved `model_id` when not present on the session row

Do not copy free-text prompt text, full source files, or patch bodies. Keep all
error text bounded (`error_text` is already truncated to 512 characters by the
exporter).

## 4. Resolve missing fields

After normalization, run the missing-field check by calling the evaluator with a
placeholder audit. Inspect `report.missing_required_fields` and
`report.sample_minima_shortfalls`:

```rust
use djinn_db::{EvalInput, WindowSpec, evaluate};

let input = EvalInput {
    window: WindowSpec { start_day, end_day, source_description },
    candidate_rows: candidate.clone(),
    baseline_rows: baseline.clone(),
    audit: None, // forces insufficient data; exposes missing fields
};
let report = evaluate(&input, None, None);
```

Remediation actions:

- `provider_id` / `model_id` missing: populate `ExportDimensions` from the
  provider/catalog resolver before calling `export_session`.
- `format_family` / `tool_surface_family` missing: set them explicitly from
  the catalog entry for the session's credential/model.
- `agent_role` missing: map the session `agent_type` to a role (e.g. `worker`,
  `reviewer`, `planner`). If a session has no role, exclude it or mark it as a
  completeness gap.
- `task_id` missing: the session must be task-scoped to be included; chat
  sessions without a task are not eligible for this population.
- `calendar_day` missing: ensure `session.started_at` is an ISO-8601 timestamp
  with at least 10 characters (`YYYY-MM-DD...`).

A row with any missing required field makes the report `insufficient data`. Do
not fabricate missing values to reach GO.

## 5. Generate the report

Build the matched baseline from a pool of rows whose `tool_surface_family` is one
of the accepted baseline families (`default` or `Responses/default`) and whose
role/task overlaps with the candidate population. Then evaluate:

```rust
use djinn_db::{matched_baseline_rows, ManualAuditResult, SampleMinima, GateThresholds};

let baseline = matched_baseline_rows(
    &candidate,
    &baseline_pool,
    &["default".into(), "Responses/default".into()],
);

let input = EvalInput {
    window,
    candidate_rows: candidate,
    baseline_rows: baseline,
    audit: Some(ManualAuditResult::new(20, 12)), // placeholder until audit completes
};

let report = evaluate(
    &input,
    Some(SampleMinima::default()),
    Some(GateThresholds::default()),
);
```

Default thresholds:

- `edit_disadvantage_points`: 5.0
- `pseudo_count_ratio`: 1.5
- `retry_disadvantage_points`: 10.0
- `audit_sample`: 20
- `audit_qualifying`: 12

The report is serializable to JSON and can be written to the decision record.

## 6. Sample 20 failed edit traces and record audit classifications

From the candidate population, select exactly failed `edit` calls (`result_status !=
"success"` or declared failure class). Sample 20 without replacement, preferring a
stratified sample across sessions and tasks if possible. For each sampled trace:

1. Record the session/task/trace IDs.
2. Classify as **genuine surface confusion / context failure** (qualifying) or
   **not** (e.g. user-cancelled, provider error, tool syntax error, unrelated
   worktree state).
3. Count qualifying cases.

Update the decision record with:

```yaml
audit:
  sampled: 20
  qualifying: 12  # or the actual count
  completed: true
```

If fewer than 20 failed edit traces exist, the audit is incomplete and the
report must be `insufficient data`.

The `run_telemetry_analysis` example computes the same deterministic sample
frame automatically: failed `edit` rows sorted by `session_id`, `task_id`,
`turn_index`, `tool_call_id`, then the first 20. Re-run the example without
`--audit-*` to emit the frame, classify the traces, and then re-run with the
`--audit-sampled` and `--audit-qualifying` flags.

## 7. Re-evaluate and commit the decision record

After the audit is complete, re-run the evaluator with the actual audit result:

```rust
let input = EvalInput {
    window,
    candidate_rows: candidate,
    baseline_rows: baseline,
    audit: Some(ManualAuditResult::new(sampled, qualifying)),
};
let report = evaluate(&input, None, None);
```

Commit a decision record that includes all fields from the report. Use the blank
template at `docs/telemetry-decision-record-template.md` and a completed
synthetic example at `docs/telemetry-decision-record-example.md`.

## Deterministic CI fixtures

The repository includes deterministic end-to-end synthetic fixtures that run in
CI. They start from synthetic `PersistedTranscript` inputs, run through
`normalize_persisted_transcript`, metric derivation, and the evaluator, and
assert GO, STOP, and insufficient-data outcomes. These fixtures do **not**
represent production evidence and do not perform production collection or manual
audit.

Run the fixture tests with:

```bash
cd server
cargo test -p djinn-db --lib repositories::tool_call_evaluator::tests::e2e_
```

## Model-facing surface guard

Phase 1 does not add, remove, rename, or change the schema/visibility of any
model-facing tool. A snapshot regression guard in `djinn-mcp-extension` pins the
default model-facing tool schemas. If the snapshot changes, the Phase 1 artifacts
must not be considered responsible for any tool-surface change.

## Privacy-safe bounded fields

The following fields are intentionally bounded or derived to avoid storing
free-text source content:

- `args_hash`: SHA-256 of canonical JSON args (not the raw args).
- `error_text`: truncated to 512 characters and normalized whitespace.
- `path`: file path only, no file content.
- `read_offset` / `read_limit`: numeric window, no file content.
- `diagnostics`: missing-field keys only.
