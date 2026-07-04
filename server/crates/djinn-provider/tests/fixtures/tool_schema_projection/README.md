# Tool-Schema Projection Corpus Fixtures

This directory holds the **committed JSON corpus** consumed by the
tool-schema projection invariant tests in
`server/crates/djinn-provider/tests/`.

Each fixture is a JSON object in **RMCP tool-definition shape**
(`name`, `description`, `inputSchema`, plus safety-annotation metadata).
The projection layer (`provider::format::tool_projection::project`)
operates on the `inputSchema` field, so fixtures preserve the full tool
object so downstream tests can extract and project exactly as the
provider serialization seams do.

## Directory layout

```
tool_schema_projection/
├── builtin/          # Snapshots of the real built-in tool schemas
│   ├── worker.json       # tool_schemas_worker()
│   ├── planner.json      # tool_schemas_planner()
│   ├── lead.json         # tool_schemas_lead()
│   ├── reviewer.json     # tool_schemas_reviewer()
│   └── architect.json    # tool_schemas_architect()
├── regression/       # Named regression schemas for known-bad shapes
│   ├── 01_empty_items_object.json
│   ├── 02_items_object_no_properties.json
│   ├── 03_allof_if_then_conditionals.json
│   ├── 04_schemars_untagged_enum_anyof.json
│   ├── 05_ref_siblings.json
│   ├── 06_tuple_prefix_items.json
│   ├── 07_unevaluated_items.json
│   ├── 08_gemini_forbidden_keywords.json
│   └── 09_combined_real_world.json
└── README.md         # This file
```

## Why committed snapshots (not direct imports)?

The `djinn-provider` crate **cannot** add `djinn-agent` or
`djinn-control-plane` as dev-dependencies without creating a dependency
cycle:

```
djinn-provider →(dev)→ djinn-agent →(runtime)→ djinn-provider   ✗ cycle
djinn-provider →(dev)→ djinn-control-plane →(runtime)→ djinn-provider  ✗ cycle
```

Both `djinn-agent` and `djinn-control-plane` depend on `djinn-provider`
at runtime (see their `Cargo.toml`). Cargo rejects even dev-dependency
cycles because `cargo test` links dev-deps into the same build graph as
runtime deps.

Therefore the corpus is committed as JSON snapshots, refreshed by an
out-of-band command that runs **outside** `djinn-provider`.

## Built-in corpus source

The `builtin/*.json` files are **snapshots** of the role-based tool
schema helpers exported by `djinn-mcp-extension` (registered via
`djinn-agent`):

| Fixture         | Source function                          |
|-----------------|------------------------------------------|
| `worker.json`   | `djinn_agent::extension::tool_schemas_worker()`   |
| `planner.json`  | `djinn_agent::extension::tool_schemas_planner()`  |
| `lead.json`     | `djinn_agent::extension::tool_schemas_lead()`     |
| `reviewer.json` | `djinn_agent::extension::tool_schemas_reviewer()` |
| `architect.json`| `djinn_agent::extension::tool_schemas_architect()`|

These are the same values captured by the insta snapshots at
`server/crates/djinn-agent/src/extension/tests/snapshots/`
(`*_tool_schemas.snap`), stripped of insta frontmatter.

### DjinnMcpServer corpus

The `DjinnMcpServer::all_tool_schemas()` tool set (147 tools as of the
last count) is runtime-constructed in `djinn-control-plane` and cannot be
imported here without a cycle. The role-based snapshots above cover the
worker/planner/lead/reviewer/architect tool surfaces that are actually
projected at provider serialization seams.

To add a `DjinnMcpServer` snapshot in the future, run the refresh command
(see below) from a crate that *can* depend on `djinn-control-plane`
(e.g. the `djinn` server binary's test suite) and commit the output as
`builtin/djinn_mcp_server.json`.

## Refresh path

### Regenerating the role-based builtin snapshots

The canonical source for these snapshots is the insta snapshot test in
`djinn-agent`:

```sh
# From the server/ workspace root:
cargo test -p djinn-agent --lib \
  extension::tests::schema_snapshot_tests::role_schema_snapshots_match_registered_role_name_source

# If the schema surface changed intentionally, accept new snapshots:
cargo insta accept --workspace \
  crates/djinn-agent/src/extension/tests/snapshots/*_tool_schemas.snap
```

Then copy the updated insta snapshots into this fixture directory:

```sh
# From the server/ workspace root:
for role in worker planner lead reviewer architect; do
  # Strip insta frontmatter (first 5 lines) and copy as JSON
  tail -n +6 \
    "crates/djinn-agent/src/extension/tests/snapshots/djinn_agent__extension__tests__schema_snapshot_tests__${role}_tool_schemas.snap" \
    > "crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/${role}.json"
done
```

### When fixtures intentionally changed

Reviewers can tell that a fixture change is **intentional** (not drift)
by checking:

1. **The `djinn-agent` insta snapshots changed in the same PR.**
   The role snapshots in
   `crates/djinn-agent/src/extension/tests/snapshots/` are the canonical
   source. If those changed and the `djinn-provider` builtin fixtures
   mirror the change, the refresh was run correctly.

2. **The commit message references the tool-surface change.**
   A new tool, renamed tool, or schema-shape change should be documented
   in the commit that updates both the source and the fixture.

3. **The regression fixtures only change for documented reasons.**
   Regression fixtures (`regression/*.json`) represent known-bad shapes
   from proposal `mpen`. They should only change when a new bad shape is
   discovered or an existing one is re-characterized — never as a side
   effect of tool-surface drift.

### Adding the DjinnMcpServer snapshot (future)

```sh
# Run from a test that can construct DjinnMcpServer (e.g. server/ tests):
# Write a test that serializes all_tool_schemas() to JSON, then:
cargo test -p djinn --test tool_schemas -- \
  serialize_djinn_mcp_server_corpus \
  --nocapture  # prints JSON to stdout, redirect to the fixture file
```

## Proposal mpen regression shapes

The `regression/` directory contains one named fixture per known-bad
schema shape called out by proposal `mpen`. Each fixture's `description`
field documents which provider(s) reject the shape and what the
projection must do:

| # | Fixture                             | Bad shape                                         | Projection action                          |
|---|-------------------------------------|---------------------------------------------------|--------------------------------------------|
| 1 | `01_empty_items_object`             | `items: {}` (permissive/untyped)                  | OpenAI: enforce schema; Gemini: keep      |
| 2 | `02_items_object_no_properties`     | `items: {"type":"object"}` no `properties`        | OpenAI: add `properties: {}`              |
| 3 | `03_allof_if_then_conditionals`     | `allOf` / `if` / `then` conditionals              | Strip or leave per family compat          |
| 4 | `04_schemars_untagged_enum_anyof`   | `anyOf` with null variant (schemars untagged enum)| Flatten null + single branch              |
| 5 | `05_ref_siblings`                   | `$ref` with sibling keywords                      | Moonshot: strip siblings                  |
| 6 | `06_tuple_prefix_items`             | tuple `prefixItems` + `items` trailing            | Moonshot: collapse to items array         |
| 7 | `07_unevaluated_items`              | `unevaluatedItems`                                | Moonshot: remove                          |
| 8 | `08_gemini_forbidden_keywords`      | Gemini-unsupported keywords + safety keys         | Gemini: whitelist filter                  |
| 9 | `09_combined_real_world`            | multiple shapes in one schema                     | All of the above applied recursively      |
