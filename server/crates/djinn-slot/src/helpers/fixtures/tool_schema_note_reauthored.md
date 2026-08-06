<!-- SYNTHETIC: body verbatim from the production note; a "## Prevention" heading was added because the real note has no ATX heading of any level. -->

Changing MCP tool schema text (param descriptions in `TaskListParams`, `shared_schemas.rs`, etc.) fans out into MULTIPLE derived golden files, and regenerating only some of them leaves `main` in a state where the full merge-queue suite fails for EVERY subsequent PR — not just the offending one.

Observed 2026-07-09: PR #1790 added `status`/`sort` descriptions to `task_list` and regenerated the 8 djinn-mcp-extension role schema `.snap`s, the server `mcp_tools_schema.snap`, and `ui/src/api/mcp-tools.gen.ts` — but missed the `djinn-agent` role snapshots (lead/planner/worker) and the `DjinnMcpServer` corpus fixture. Those stale goldens failed the merge-queue suite for every PR until PR #1795 regenerated them.

**Checklist when touching tool schemas** (grep for all of these):
- djinn-mcp-extension role schema `.snap`s
- server `mcp_tools_schema.snap`
- `djinn-agent` role snapshots (lead/planner/worker)
- `DjinnMcpServer` corpus fixture
- `ui/src/api/mcp-tools.gen.ts` (CI check: "Generated MCP types up to date")

Also: when several concurrent PRs touch tool schemas, golden-file collisions make the merge queue thrash. Minimize the collision surface — behavior-only changes (e.g. runtime SQL like LIKE→ILIKE) can deliberately skip doc-string edits to produce zero generated-file churn.


## Prevention
**Concrete regen commands** (bit again 2026-07-13, PR #2054 — GraphNode.created_at OUTPUT-schema change; output-schema edits skip the role snapshots but still hit the server snap, corpus fixture, and UI types):
- Server insta snap: `INSTA_UPDATE=always cargo test --all-features tool_schemas` (in `server/`)
- Corpus fixture: `UPDATE_DJINN_MCP_SERVER_FIXTURE=1 cargo test -p djinn-control-plane --lib server_tests::tests::djinn_mcp_server_corpus_fixture_is_current` → writes `crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json` (fails CI as "Server Test shard" — easy to miss because it is NOT named a schema check)
- UI types: `pnpm mcp:types:snapshot` (in `ui/`; reads the server insta snap, so regenerate that FIRST)
- If SQL queries changed too: `make sqlx-prepare` (needs `docker start djinn-postgres-test`)
- Role snapshots (djinn-mcp-extension + djinn-agent) only carry INPUT schemas — output-only changes leave them untouched (verify with `grep -rl <new-field> --include='*.snap'`).

## Notes


Confirmed 2026-07-13 (epics.proposal_id / EpicModel proposal_* enrichment): adding new `Option<T>` output fields with `#[serde(skip_serializing_if = "Option::is_none")]` also leaves the server contract response snaps (`contracts__epic_list_response.snap`, `contracts__epic_show_response.snap`) and `djinn-mcp-extension/tests/fixtures/tool_surface_baseline.json` untouched — None fields never serialize. Only the 3 output-schema goldens (server insta snap → UI types → corpus fixture) needed regen.



**The corpus fixture's own crate cannot tell you it is stale** (bit again 2026-07-25, PR #2606 — `EnvironmentConfig` gained `evidence_tier` + `volatile_environment_names`). The fixture FILE lives in `djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json`, but its CURRENCY is asserted only from `djinn-control-plane` (`server_tests::tests::djinn_mcp_server_corpus_fixture_is_current`).

`djinn-provider`'s own suite — `tool_schema_corpus_loader`, `tool_schema_projection_corpus` — passes green against a **stale** fixture, because it only checks shape, size, tool-name uniqueness, and projection identity. Never currency.

So this reasoning is invalid and cost a full CI cycle: *"I regenerated the server snap and the UI types, then ran `cargo test -p djinn-provider --test tool_schema_projection_corpus` and it was green, therefore the fixture is current."* It is green either way.

Verify the fixture by running the assertion that owns it, not the crate that stores it:

```
cargo nextest run -p djinn-control-plane -E 'test(djinn_mcp_server_corpus_fixture_is_current)'
```

General form of the trap: when a golden's file location and its freshness assertion live in different crates, testing the crate that HOLDS the file proves nothing. Grep for the regen env var (`UPDATE_DJINN_MCP_SERVER_FIXTURE`) to find the crate that actually owns the check.



**Adding a NEW agent-role tool hits a DIFFERENT set than editing schema text** (observed 2026-07-30, PR #2805 — `set_file_mode` on the worker surface).

Two disjoint tool surfaces exist, and confusing them wastes a regen cycle:

- **agent-role surface** (`djinn-mcp-extension/src/tool_defs.rs`, `tool_schemas_worker()` etc.) — what worker/reviewer/planner/architect models see.
- **djinn MCP server surface** — what external MCP clients see. This is the one that fans out to `mcp_tools_schema.snap` → `ui/src/api/mcp-tools.gen.ts` → the `DjinnMcpServer` corpus fixture, per the checklist above.

Adding a tool to the **agent-role** surface touched **none** of those three. It moved seven other things:

1. `djinn-mcp-extension/src/tests/snapshots/…__worker_tool_names.snap`
2. `djinn-mcp-extension/src/tests/snapshots/…__worker_tool_schemas.snap`
3. `djinn-agent/src/extension/tests/snapshots/…__schema_snapshot_tests__worker_tool_schemas.snap` (a SECOND copy of the worker schemas, in a different crate)
4. `djinn-agent/src/snapshots/…__prompts__tests__worker_tools_section_snapshot.snap` (the tools list rendered into the role PROMPT)
5. `djinn-mcp-extension/tests/fixtures/tool_surface_baseline.json` — regen with `cargo run -p djinn-mcp-extension --bin regenerate_tool_surface_baseline`; the failure message names the command, but it is a `--test tool_surface_baseline` integration test, easy to miss under a `--lib` run
6. `djinn-mcp-extension/src/tests/schema_tests.rs` — `expected_safety_tuple()`. **Not a golden file**: a hand-maintained Rust `match` over tool names. `INSTA_UPDATE=always` cannot fix it; a new tool fails with `worker tool <name> is missing pinned safety classification` and needs a match arm choosing read_only / mutation / idempotent_mutation / destructive / open_world.
7. Any role surface the tool is added to — keep it to the narrowest role set to hold the churn down (`set_file_mode` is worker-only, so the reviewer/planner/architect/judge snapshots stayed untouched).

Two lessons generalizing past this case:

- `INSTA_UPDATE=always cargo test -p <crate>` leaves `.snap.new` files behind when the run also had a non-insta failure. Check `git status` for `*.snap.new` and either `mv` them over the `.snap` or re-run cleanly — a stray `.snap.new` is untracked, so it silently does not fix the failing check.
- The same logical golden (worker tool schemas) is stored **twice, in two crates**. Regenerating one and running only that crate's tests reads green. Always finish with `cargo test -p djinn-mcp-extension -p djinn-agent` before believing a tool-surface change is complete.