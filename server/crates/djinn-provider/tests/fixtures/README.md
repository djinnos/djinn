# Test Fixtures

This directory holds committed JSON/text fixtures consumed by
`djinn-provider` integration tests.

## Subdirectories

- [`tool_schema_projection/`](tool_schema_projection/README.md) —
  Tool-schema projection corpus: built-in role tool schema snapshots and
  proposal `mpen` regression shapes for known-bad JSON Schema patterns.
  See its README for the refresh path and the dependency-cycle rationale
  for using committed snapshots instead of direct crate imports.
