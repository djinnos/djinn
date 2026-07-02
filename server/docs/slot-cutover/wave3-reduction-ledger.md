# Wave 3 Reduction Ledger — Slice dzog

## Task

019f23f0-c130-7d21-93cd-74432e30e2e0 — Parameterize slot provider and MCP resolution helpers.

## Scope and method

Measured paths (from `server/`):

```bash
find crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
find crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
```

## Baseline (post-iwap / pre-dzog)

```bash
$ find crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
9467

$ find crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
28622

$ find crates/djinn-agent/src/actors/slot crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
38089
```

## After dzog

```bash
$ find crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
9166

$ find crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
28622

$ find crates/djinn-agent/src/actors/slot crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
37788
```

| Tree | Before | After | Delta |
|---|---|---:|---:|
| `crates/djinn-agent/src/actors/slot` | 9467 | 9166 | -301 |
| `crates/djinn-slot/src` | 28622 | 28622 | 0 |
| **Combined** | **38089** | **37788** | **-301** |

## Touched files

- `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs`
- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs`
- `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` (deleted)
- `server/crates/djinn-agent/src/actors/slot/lifecycle/mcp_resolve.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/role_overrides.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs`
- `server/crates/djinn-agent/src/actors/slot/mod.rs`
- `server/crates/djinn-agent/src/direct_services.rs`
- `server/crates/djinn-agent/src/supervisor_impl/stage.rs`

## What changed

### Provider resolution helpers

- Consolidated near-duplicate Codex and Copilot OAuth load/refresh logic between `load_provider_credential` and `refresh_oauth_credential_after_401` into private helpers `load_or_refresh_codex` and `load_or_refresh_copilot`.
- Extracted shared `effective_oauth_provider_id` helper.
- Collapsed duplicated `AuthMethod`/`FormatFamily` match arms in `OAuthConfigWire` via helper methods on the wire enums.
- Removed `build_telemetry_meta`; callers now use `build_telemetry_meta_with_attribution(..., None, None)` directly.
- Trimmed verbose historical comments.

### MCP resolution

- Replaced monolithic `resolve_mcp_and_skills` with parameterized private helpers: `resolve_mcp_server_entries`, `connect_mcp_registry`, and `load_project_skills`.
- Removed duplicate test cases and consolidated the four non-planner role tests into a single data-driven test.
- Shortened module-level doc comment.

### Role override resolution

- Extracted specialist/tribunal override logic into `resolve_runtime_role_override`.
- Shortened struct and module doc comments.

### Facade / caller updates

- Updated `direct_services.rs` and `supervisor_impl/stage.rs` to call `build_telemetry_meta_with_attribution` directly.
- Removed stale re-exports of `build_role_code_graph_context` and `build_reviewer_diff_context` from `helpers/mod.rs` and the corresponding comment in `slot/mod.rs`.
- Deleted `helpers/tests.rs` stub; canonical behavioral tests live in `djinn-slot/src/helpers/tests.rs`.

## Validation

Commands run from `server/`:

| Command | Outcome |
|---|---|
| `cargo fmt -p djinn-agent` | applied |
| `cargo clippy -p djinn-agent --all-features` | passed |
| `cargo test -p djinn-agent --all-features mcp_resolve` | 9 passed, 0 failed |
| `cargo test -p djinn-agent --all-features provider_resolution` | 5 passed, 0 failed |
| `cargo test -p djinn-agent --all-features role_overrides` | failed on ConnectionRefused (DB-dependent; no Postgres reachable) |

## Environment limitations

- DB-dependent tests (`role_overrides`) fail on `ConnectionRefused` in this sandbox, consistent with prior wave notes. No DB service is reachable and no tests were disabled or weakened.
- `cargo check` is disallowed for the worker role; `cargo clippy -p <crate>` was used as the type-checking/lint fallback.

## Line-count verdict

Combined scoped Rust line count decreased by 301 lines from the post-iwap baseline, meeting the >=300-line target for this slice.

Memory reference: [[reference/wave-3-reduction-ledger-dzog-slice]].
