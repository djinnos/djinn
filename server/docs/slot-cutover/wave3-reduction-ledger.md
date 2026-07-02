# Wave 3 reduction ledger

## Slice: lifecycle prompt-context and CI directive test scaffolding

Task: `019f23f0-661c-7f00-b928-ba71e0349301` — Reduce duplicated slot lifecycle prompt and CI directive test scaffolding.

### Line-count proof

Commands from the task description:

```sh
find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
find server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
```

Before this slice:

- `server/crates/djinn-agent/src/actors/slot`: 9,433 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 38,055 lines

After this slice:

- `server/crates/djinn-agent/src/actors/slot`: 8,713 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,335 lines

Net delta: **-720 combined scoped Rust lines**.

### Touched files

- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context_tests.rs`
  - Replaced repeated prompt-context DB/task setup and repeated section assertions with shared fixture helpers and table-style assertions.
  - Preserved coverage for epic blocker/sibling rendering, absent sections, activity formatting, conflict formatting, prompt-section ordering, no-epic roles, knowledge context fallback, and direct helper behavior.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/ci_directive_tests.rs`
  - Collapsed repeated CI task construction and role-specific prompt assertions into shared helper/table-driven tests.
  - Preserved coverage for structured CI directive rendering, audit-log non-scraping, absence cases, optional/default fields, and sa4x stability/deduplication behavior.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/test_support.rs`
  - Added private test-only fixture helpers for lifecycle prompt-context tests.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs`
  - Added the test-only support module declaration; no public `djinn_agent::actors::slot` exports changed.

### Validation

Formatting:

```sh
cargo fmt --manifest-path server/Cargo.toml
```

Result: passed; formatting applied to the edited Rust files.

Focused compile/pure CI directive tests:

```sh
OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent lifecycle::prompt_context::ci_directive_tests::build_ci_blocking_directive --lib
```

Result: passed — 3 tests passed, 0 failed.

Focused pure prompt helper tests:

```sh
OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent lifecycle::prompt_context::tests::format_ --lib
OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent lifecycle::prompt_context::tests::apply_prompt_sections_cases --lib
```

Result: passed — 3 total tests passed, 0 failed.

Environment limitations encountered:

- Running cargo without `OPENSSL_NO_VENDOR=1` failed before compiling this crate because the container does not have `make`, which `openssl-src` needs for vendored OpenSSL.
- Running the broader focused lifecycle prompt-context test filter compiled successfully with `OPENSSL_NO_VENDOR=1`, but DB-backed tests failed at fixture setup because the local Postgres sidecar lacks the expected `djinn_test_template` database:
  - `template database "djinn_test_template" does not exist`
- Because that DB template is unavailable in this session, the strongest local fallback was the successfully compiled and passing pure helper/directive subset above.

---

## Slice: provider and MCP resolution helper parameterization

Task: `019f23f0-c130-7d21-93cd-74432e30e2e0` — Parameterize slot provider and MCP resolution helpers.

### Line-count proof

Commands from the task description:

```sh
find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
find server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
```

Post-iwap / pre-dzog baseline (from the iwap slice above):

- `server/crates/djinn-agent/src/actors/slot`: 8,713 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,335 lines

After this slice (current workspace with dzog changes applied):

- `server/crates/djinn-agent/src/actors/slot`: 8,505 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,127 lines

Net delta for this slice: **-208 combined scoped Rust lines**.

Reconciliation note: the original dzog submission measured against a local post-iwap baseline of 9,467 lines and reported a 301-line reduction. The merged iwap slice actually landed at 8,713 lines. After reconciling the ledgers, the dzog contribution in this checkout is 208 lines (8,713 → 8,505). The combined iwap + dzog Wave 3 reduction from the pre-Wave-3 baseline is 928 lines (9,433 → 8,505).

### What changed

- `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs`
  - Consolidated near-duplicate Codex and Copilot OAuth load/refresh logic between `load_provider_credential` and `refresh_oauth_credential_after_401` into private helpers `load_or_refresh_codex` and `load_or_refresh_copilot`.
  - Extracted shared `effective_oauth_provider_id` helper.
  - Collapsed duplicated `AuthMethod`/`FormatFamily` match arms in `OAuthConfigWire` via helper methods on the wire enums.
  - Removed `build_telemetry_meta`; callers now use `build_telemetry_meta_with_attribution(..., None, None)` directly.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/mcp_resolve.rs`
  - Replaced monolithic `resolve_mcp_and_skills` with parameterized private helpers: `resolve_mcp_server_entries`, `connect_mcp_registry`, and `load_project_skills`.
  - Removed duplicate test cases and consolidated non-planner role tests into a single data-driven test.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/role_overrides.rs`
  - Extracted specialist/tribunal override logic into `resolve_runtime_role_override`.
- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs`
  - Removed stale re-exports of `build_role_code_graph_context` and `build_reviewer_diff_context`.
- `server/crates/djinn-agent/src/actors/slot/mod.rs`
  - Removed corresponding stale comment references.
- `server/crates/djinn-agent/src/direct_services.rs` and `server/crates/djinn-agent/src/supervisor_impl/stage.rs`
  - Updated callers to use `build_telemetry_meta_with_attribution` directly.
- `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` (deleted)
  - Stub removed; canonical behavioral tests live in `djinn-slot/src/helpers/tests.rs`.

### Touched files

- `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs`
- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/mcp_resolve.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/role_overrides.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs`
- `server/crates/djinn-agent/src/actors/slot/mod.rs`
- `server/crates/djinn-agent/src/direct_services.rs`
- `server/crates/djinn-agent/src/supervisor_impl/stage.rs`
- `server/docs/slot-cutover/wave3-reduction-ledger.md`

### Validation

Formatting:

```sh
cargo fmt -p djinn-agent
```

Result: applied.

Type-check / lint:

```sh
cargo clippy -p djinn-agent --all-features
```

Result: passed.

Focused tests:

| Command | Outcome |
|---|---|
| `cargo test -p djinn-agent --all-features mcp_resolve` | 9 passed, 0 failed |
| `cargo test -p djinn-agent --all-features provider_resolution` | 5 passed, 0 failed |
| `cargo test -p djinn-agent --all-features role_overrides` | failed on `ConnectionRefused` (DB-dependent; no Postgres reachable) |

Environment limitations encountered:

- DB-dependent tests (`role_overrides`) fail on `ConnectionRefused` in this sandbox, consistent with prior wave notes. No DB service is reachable and no tests were disabled or weakened.

### Line-count verdict

The dzog slice reduces the combined scoped Rust line count by 208 lines when measured against the reconciled post-iwap baseline of 8,713 lines. The original dzog workspace reported a 301-line reduction against a different local post-iwap baseline. The combined iwap + dzog Wave 3 reduction is 928 lines from the pre-Wave-3 baseline.

Memory reference: [[reference/wave-3-reduction-ledger-dzog-slice]].
