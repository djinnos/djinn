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
