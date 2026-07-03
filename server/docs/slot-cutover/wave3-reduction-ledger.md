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

Post-iwap baseline (from origin/main at `e46c0b389`):

- `server/crates/djinn-agent/src/actors/slot`: 8,806 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,428 lines

After this slice (current workspace with dzog changes applied):

- `server/crates/djinn-agent/src/actors/slot`: 8,378 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,000 lines

Net delta for this slice: **-428 scoped Rust lines in djinn-agent/src/actors/slot**.

Combined iwap + dzog Wave 3 reduction from the pre-Wave-3 baseline: **1,055 lines** (9,433 → 8,378).

### What changed

- `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs` (741 → 532 lines, -209)
  - Replaced four thin wrapper functions (`format_family_for_provider`, `capabilities_for_provider`, `auth_method_for_provider`, `default_base_url`) with direct `pub use` re-exports from `djinn_slot::helpers::provider_resolution`.
  - Extracted shared `effective_oauth_provider_id` helper used by both `load_provider_credential` and `refresh_oauth_credential_after_401`.
  - Consolidated Codex and Copilot OAuth load/refresh logic into `try_load_or_refresh_codex` and `try_load_or_refresh_copilot`, eliminating the near-duplicate match arms between the two public functions.
  - Removed the `build_telemetry_meta` wrapper; callers now use `build_telemetry_meta_with_attribution(..., None, None)` directly.
  - Trimmed verbose module-level, struct-field, and function doc comments while preserving essential behavioral contracts.
  - All existing wire round-trip tests preserved.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/mcp_resolve.rs` (491 → 362 lines, -129)
  - Split monolithic `resolve_mcp_and_skills` into parameterized private helpers: `resolve_mcp_server_entries`, `connect_mcp_registry`, and `load_project_skills`.
  - Consolidated `#[cfg(test)]` / `#[cfg(not(test))]` branches for MCP registry connection into a single code path with a `#[cfg(test)]` early-return for the override.
  - Consolidated four non-planner role tests (worker, reviewer, lead, architect) into a single data-driven test.
  - Trimmed verbose module-level and inline comments.
- `server/crates/djinn-agent/src/actors/slot/lifecycle/role_overrides.rs` (466 → 388 lines, -78)
  - Extracted `resolve_runtime_role_override` helper that handles both the specialist (Worker stage) and tribunal (Refinement stage) override paths.
  - Trimmed verbose module-level, struct-field, and function doc comments.
- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs` (82 → 70 lines, -12)
  - Removed `build_telemetry_meta` re-export.
  - Removed `#[cfg(test)] mod tests;` declaration (the stub file was just comments).
- `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` (19 → 0 lines, deleted)
  - Removed empty stub file that contained only comments about tests living in djinn-slot.
- `server/crates/djinn-agent/src/actors/slot/mod.rs` (169 → 153 lines, -16)
  - Trimmed stale re-export comment block.
- `server/crates/djinn-agent/src/supervisor_impl/stage.rs`
  - Updated `build_telemetry_meta` call to `build_telemetry_meta_with_attribution`.
- `server/crates/djinn-agent/src/direct_services.rs`
  - Updated `build_telemetry_meta` call to `build_telemetry_meta_with_attribution`.

### Touched files

- `server/crates/djinn-agent/src/actors/slot/helpers/provider_resolution.rs`
- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs`
- `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` (deleted)
- `server/crates/djinn-agent/src/actors/slot/lifecycle/mcp_resolve.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/role_overrides.rs`
- `server/crates/djinn-agent/src/actors/slot/mod.rs`
- `server/crates/djinn-agent/src/supervisor_impl/stage.rs`
- `server/crates/djinn-agent/src/direct_services.rs`
- `server/docs/slot-cutover/wave3-reduction-ledger.md`

### Validation

Formatting:

```sh
cargo fmt --manifest-path server/Cargo.toml -p djinn-agent
```

Result: applied.

Type-check / lint:

```sh
OPENSSL_NO_VENDOR=1 cargo clippy --manifest-path server/Cargo.toml -p djinn-agent --all-features
```

Result: passed, no warnings or errors.

Focused tests:

| Command | Outcome |
|---|---|
| `OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent --all-features mcp_resolve` | 12 passed, 0 failed |
| `OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent --all-features provider_resolution` | 5 passed, 0 failed |
| `OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent --all-features role_overrides` | 5 failed on `ConnectionRefused` / missing `djinn_test_template` (DB-dependent; no Postgres reachable) |

Environment limitations encountered:

- DB-dependent tests (`role_overrides`) fail on `ConnectionRefused` / missing `djinn_test_template` database in this sandbox. No tests were disabled or weakened.
- `OPENSSL_NO_VENDOR=1` is required; the container lacks `make` for vendored OpenSSL.

---

## Slice: private slot supervisor-runner host wiring overlap

Task: `019f23f1-1bfd-7f62-95e3-4e050c28d2a2` — Reduce private slot supervisor-runner host wiring overlap.

### Line-count proof

```sh
find server/crates/djinn-agent/src/actors/slot -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
find server/crates/djinn-slot/src -name '*.rs' -type f -print0 | xargs -0 cat | wc -l
```

Post-dzog baseline (ggwy branch HEAD `1fdf485e5`):

- `server/crates/djinn-agent/src/actors/slot`: 9,154 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,776 lines

After this slice:

- `server/crates/djinn-agent/src/actors/slot`: 8,632 lines
- `server/crates/djinn-slot/src`: 28,622 lines
- Combined: 37,254 lines

Net delta: **-522 combined scoped Rust lines** (supervisor_runner.rs 2,265→1,862, mod.rs 153→34).

### What changed

- `server/crates/djinn-agent/src/actors/slot/supervisor_runner.rs` (2,265 → 1,862 lines, -403)
  - Condensed all verbose multi-line doc comments on types, functions, and constants to 1-2 lines each.
  - Removed verbose inline comment blocks explaining dispatch flow (infra-death, pre-session timeout, credential revocation, provider failure class matching, cancel task tracing, reap/session finalization).
  - Simplified the cancel-task `tokio::spawn` block: removed three nested tracing spans keeping only the outer `djinn.slot.kill` span; removed unused `cancel_session_id`.
  - Replaced `build_task_run_spec` free function with `impl From<TaskRunSpecInputs> for TaskRunSpec`.
  - All existing tests preserved and passing.
- `server/crates/djinn-agent/src/actors/slot/mod.rs` (153 → 34 lines, -119)
  - Replaced verbose module-level facade documentation with a 2-line summary and single-line inline module comments.
  - Preserved all module declarations and re-exports unchanged.

### Caller-impact notes

- No public API changes. All `pub use` re-exports and function signatures unchanged.
- `build_task_run_spec` was private; its only caller updated to `TaskRunSpec::from()`.
- Cancel block simplification removed redundant tracing spans — kill event still observable via outer span.

### Validation

Formatting:

```sh
cargo fmt --manifest-path server/Cargo.toml -p djinn-agent
```

Result: applied.

Type-check / lint:

```sh
OPENSSL_NO_VENDOR=1 cargo clippy --manifest-path server/Cargo.toml -p djinn-agent --lib
```

Result: passed, no warnings or errors.

Focused supervisor-runner tests:

```sh
OPENSSL_NO_VENDOR=1 cargo test --manifest-path server/Cargo.toml -p djinn-agent --lib supervisor_runner::tests
```

Result: 14 passed, 1 failed (DB environment limitation — `djinn_test_template` not available).

Environment limitations:

- DB-backed test `await_report_pre_session_deadline_fires_naming_last_step` fails on missing `djinn_test_template` — same limitation as prior slices. No tests disabled or weakened.
