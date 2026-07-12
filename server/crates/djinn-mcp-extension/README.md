# MCP tool-surface baseline

This crate owns the deterministic, reviewable contract for the Djinn-owned
agent-facing MCP tool surface.

## What is included in the baseline

The checked-in fixture [`tests/fixtures/tool_surface_baseline.json`](./tests/fixtures/tool_surface_baseline.json)
is the canonical, byte-for-byte contract for every tool that is currently
advertised to an active Djinn role or session. It is generated from the active
schema aggregators in `src/tool_defs.rs`:

- `tool_schemas_worker`
- `tool_schemas_reviewer`
- `tool_schemas_lead`
- `tool_schemas_planner`
- `tool_schemas_architect`
- `tool_schemas_advocate`
- `tool_schemas_adversary`
- `tool_schemas_judge`
- `tool_schemas_evidence_spike`

Each aggregator is production code: a role or session surface change that adds,
removes, or renames a tool shows up in the generated fixture, and the CI test
will fail if the fixture is not updated.

The canonical projection keeps only the advertised surface fields:

- `name`
- `description`
- `inputSchema`
- `readOnly`
- `destructive`
- `idempotent`
- `openWorld`
- `concurrent_safe`

Everything else is stripped. The fixture is recursively sorted, sorted by tool
name, and pretty-printed with a trailing newline.

## What is intentionally excluded

The baseline is a snapshot of *advertised* tools only. Compatibility-only tools
that are no longer emitted by any active aggregator are not included.

`tool_request_lead` in `src/tool_defs.rs` is the only intentionally excluded
callable definition. It is a `[HISTORICAL-COMPAT]` drain-window tool retained
after epic `10qg` so that stale sessions dispatched before the cut-over can still
invoke it and be routed to the deprecated compatibility handler. No active
role/session aggregator advertises it, so it is not part of the review contract
and it does not appear in the fixture. The unit tests in `src/tool_surface.rs`
assert that `request_lead` is absent from the canonical surface.

## Default feature and configuration assumptions

The baseline is generated with the crate's default feature set. The generation
and test code do not enable any special feature flags or environment variables:

- No `--features` flag is required.
- `SQLX_OFFLINE` is not consulted by the generator.
- The generator runs in-process and does not touch the network, the filesystem
  (other than the regeneration binary), or the build cache.
- The exact output depends only on the schema definitions, the `serde_json`
  canonicalization, and the safety annotations produced by
  `shared_schemas::annotate_tool_safety`.

Regeneration and the test both run with the default Cargo profile. Debug versus
release compilation does not change the fixture bytes.

## Regeneration command

Regenerate the fixture from the current source code with:

```text
cargo run -p djinn-mcp-extension --bin regenerate_tool_surface_baseline
```

This binary uses the same public API as the regression test
(`djinn_mcp_extension::tool_surface::tool_surface_baseline_json`). Do not hand-edit
`tool_surface_baseline.json`; the JSON must be produced by the shared generator
so that the test and the regeneration binary cannot drift.

## Reviewer workflow

When a PR touches the tool surface, the CI failure will be a byte-level mismatch
between the generated fixture and `tool_surface_baseline.json`. Review the PR as
follows:

1. Read the `name`/`description`/`inputSchema` diff of the failing fixture.
2. Confirm the change is intentional and matches the role/session aggregation
   intent in `src/tool_defs.rs`.
3. Check that the operation-enum guard in the test passes. If it fails, read the
   diagnostics: each diagnostic identifies the tool, the exact field path, the
   value mentioned in the description that is missing from the enum, and the
   nearest enum values.
4. If the change is approved, run the regeneration command and commit the updated
   fixture. Do not edit the JSON manually.

## Expected fixture changes vs. unexpected drift

Expected changes (regenerate and review):

- A new tool is added to an active aggregator.
- An existing tool's description is reworded.
- A tool gains or loses a required parameter, or an `inputSchema` property changes.
- A safety annotation changes because a tool's role is reclassified.
- The tool set changes when a role's surface is intentionally expanded or
  restricted (e.g. evidence-spike allowlist changes).

Unexpected drift (investigate, do not regenerate blindly):

- A tool name changes without an explicit role cut-over decision.
- `inputSchema` changes that are not reflected in the description.
- A tool's `operation` enum and description fall out of sync (the operation-enum
  guard will fail).
- A difference in the generated output that is not explained by a source change
  in `src/tool_defs.rs` or `src/shared_schemas.rs`.

## Operation-enum guard failures

The integration test runs the operation-enum guard on the generated tool surface
before comparing it to the fixture. The guard recursively walks every string
property named `operation` with an `enum` array and checks that values mentioned
in the owning tool's description are present in the enum.

The guard extracts candidate values only from:

- Markdown code spans (`\`value\``) when the surrounding context refers to an
  operation or the value is already in the enum.
- Slash-separated lists such as `ranked/cycles/orphans/path/edges` when at least
  two values match the enum, or the line is explicitly labeled as an operation
  list.
- Comma-separated lists that follow an explicit `operation:` / `operations:` /
  `operation=` label.

Ordinary prose (e.g. "this operation can route requests through a flow") is not
treated as a catalogue, so the guard should not produce false positives from
natural language.

When the guard fails, the test output reports each mismatch with the tool name,
JSON field path, the missing value (case preserved), and up to three nearest
enum values by edit distance. Fix the mismatch by adding the value to the enum,
updating the description, or adding a reviewed allowlist entry in
`OPERATION_DESCRIPTION_ALLOWLIST` with a documented rationale.

The allowlist is intentionally empty; adding an entry is a contract decision,
not a parser escape hatch.

## CI coverage

The baseline test is an integration test in `tests/tool_surface_baseline.rs`. It
is automatically discovered and executed by the existing unfiltered workspace
Rust test job in `.github/workflows/quality-gate.yml`:

```text
cargo nextest run --workspace --all-targets --features qdrant --profile ci --partition count:${{ matrix.shard }}/4
```

No extra shard, duplicate job, or fixed tool-count assertion is required. The
unfiltered command already builds and runs all package integration tests,
including the one in this crate. Discovery was verified locally with:

```text
cargo nextest list --all-targets --workspace --features qdrant -p djinn-mcp-extension
```

which lists `djinn-mcp-extension::tool_surface_baseline generated_tool_surface_matches_reviewed_baseline`.
The CI workflow is therefore left unchanged.
