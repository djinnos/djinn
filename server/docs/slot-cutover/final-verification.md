# Slot Cut-over Final Verification

> Generated: 2026-07-01  
> Task: `019f1d87-a377-7781-9264-b819f4648888` — Audit and consolidate canonical slot behavior test homes

## Inputs and source-of-truth note

This checkout does not contain the broad foundation inventory files expected at
`server/docs/slot-cutover/test-inventory.md`, `server/docs/slot-cutover/README.md`,
or `server/docs/slot-cutover/baseline-inventory.md`. Per the verification-task
instruction, this file records current-source evidence instead of recreating that
foundation inventory.

The available facade inventory was consulted at
`server/crates/djinn-agent/docs/slot-facade-inventory.md`. It records the p6i4
state: `djinn-agent/src/actors/slot` is a compatibility facade containing
host-only `AgentContext` wiring, thin `AgentContext -> SlotContext` shims, and
facade/host tests; canonical slot implementation behavior lives in `djinn-slot`.

## Canonical test-home map

| Behavior area | Canonical home | Agent-side status after audit | Rationale |
|---|---|---|---|
| LLM/session extraction behavior (structural extraction, LLM note generation, dedup/novelty, graceful provider failure, note persistence/provenance) | `server/crates/djinn-slot/src/llm_extraction_tests.rs` | Consolidated: removed duplicate agent registration and duplicate file `server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs`. | The agent file asserted the same extraction behavior through test-only `AgentContext` adapters in `server/crates/djinn-agent/src/actors/slot/llm_extraction.rs`; those assertions are slot behavior, not pure facade compatibility. Canonical coverage remains in `djinn-slot/src/llm_extraction_tests.rs`. |
| Top-level slot helpers behavior (merge-conflict parsing, provider/model routing helpers, text/command formatting, recent feedback, initial user message and combined reviewer/CI budget behavior) | `server/crates/djinn-slot/src/helpers_tests.rs` | Consolidated: removed duplicate agent registration and duplicate file `server/crates/djinn-agent/src/actors/slot/helpers_tests.rs`. | The removed agent file was behavior-equivalent to the slot file and exercised the same helper assertions through the facade path. Canonical coverage remains in `djinn-slot/src/helpers_tests.rs`. |
| Reply-loop smoke/integration behavior (text-only completion, tool result turn insertion, finalize detection, empty-turn nudge, max-nudge error, provider error propagation, safe/unsafe tool dispatch, ordered mixed tool results) | `server/crates/djinn-slot/src/reply_loop_tests.rs` | No duplicate agent test module is registered. `server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs` documents that behavior coverage lives in `djinn-slot/src/reply_loop_tests.rs`; agent compatibility for this thin shim is compile-time facade coverage. | These are canonical reply-loop behaviors and already have one home in `djinn-slot`. |
| Module-local helper tests (code graph auto-context and reviewer-diff context helpers) | `server/crates/djinn-slot/src/helpers/tests.rs` for slot helper behavior. | Retained: `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` remains registered from `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs`. | The agent helper module is retained as host-only facade surface per `slot-facade-inventory.md` and depends on `AgentContext`/agent test helpers. Its local tests are retained as host-only compatibility coverage for the agent facade; canonical slot helper behavior remains covered in `djinn-slot/src/helpers/tests.rs`. |
| Module-local reply-loop tests (compaction, serialization/order, finalize/tool-choice, nudge/guard, budget/wind-down behavior) | `server/crates/djinn-slot/src/reply_loop/tests.rs` | No corresponding agent module-local tests are registered. `server/crates/djinn-agent/src/actors/slot/reply_loop/mod.rs` is a thin adapter/re-export module. | Pure reply-loop behavior belongs in `djinn-slot`; the agent module only adapts `AgentContext` and host tool-dispatch glue. |
| Pool tests (metrics aggregation and slot snapshot state) | `server/crates/djinn-slot/src/pool/tests.rs` | No corresponding agent `pool/tests.rs` exists in this checkout. Agent `pool/` remains host-only facade/wrapper code (`SlotPoolHandle`, `SlotFactory`, status types) and has no duplicate behavior test home. | Canonical pool behavior assertions have a single checked-in source path in `djinn-slot`. |
| Agent lifecycle/prompt/facade tests | Host-only agent paths: `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context_tests.rs`, `server/crates/djinn-agent/src/actors/slot/lifecycle/ci_directive_tests.rs`, inline tests in `lifecycle/*`, `finalize_handlers.rs`, `actor.rs`, `supervisor_runner.rs`, and retained `helpers/tests.rs`. | Retained. | These are not broad duplicate slot-behavior homes; they cover agent-only lifecycle prompt assembly, CI directives, facade adapter behavior, host dispatch, or compatibility exports and should not be deleted by this task. |

## Consolidation performed in this task

- Removed `#[cfg(test)] mod helpers_tests;` and `#[cfg(test)] mod llm_extraction_tests;` from `server/crates/djinn-agent/src/actors/slot/mod.rs`.
- Deleted `server/crates/djinn-agent/src/actors/slot/helpers_tests.rs` because the same slot helper behavior assertions live canonically in `server/crates/djinn-slot/src/helpers_tests.rs`.
- Deleted `server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs` because the same LLM/session extraction behavior assertions live canonically in `server/crates/djinn-slot/src/llm_extraction_tests.rs`.

No host-only agent tests were deleted. The retained agent tests cover agent facade/host-only wiring rather than independent slot behavior implementations.

## Current test registration evidence

Current checked-in source registers the canonical slot behavior homes from
`server/crates/djinn-slot/src/lib.rs`:

- `mod helpers_tests;` -> `server/crates/djinn-slot/src/helpers_tests.rs`
- `mod llm_extraction_tests;` -> `server/crates/djinn-slot/src/llm_extraction_tests.rs`
- `mod reply_loop_tests;` -> `server/crates/djinn-slot/src/reply_loop_tests.rs`

Module-local slot tests remain registered from:

- `server/crates/djinn-slot/src/helpers/mod.rs` -> `server/crates/djinn-slot/src/helpers/tests.rs`
- `server/crates/djinn-slot/src/reply_loop/mod.rs` -> `server/crates/djinn-slot/src/reply_loop/tests.rs`
- `server/crates/djinn-slot/src/pool/mod.rs` -> `server/crates/djinn-slot/src/pool/tests.rs`

Agent-side retained test registrations are host/facade homes, not duplicate broad
slot-behavior homes:

- `server/crates/djinn-agent/src/actors/slot/helpers/mod.rs` -> `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs`
- `server/crates/djinn-agent/src/actors/slot/lifecycle/prompt_context.rs` -> `prompt_context_tests.rs` and `ci_directive_tests.rs`
- Inline tests remain in host/facade modules such as `finalize_handlers.rs`, `actor.rs`, `lifecycle/*`, and `supervisor_runner.rs`.

## Validation log

Commands were run from `server/`.

| Command | Outcome |
|---|---|
| `cargo fmt --package djinn-slot --package djinn-agent` | Passed; no formatter errors. |
| `cargo test -p djinn-slot --lib helpers_tests` | Built and ran the filtered lib tests, but failed because the local Postgres service refused connections. Result: 11 passed, 4 failed; failures reported `Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))`. |
| `cargo test -p djinn-slot --lib llm_extraction_tests` | Built and ran the filtered lib tests, but failed because the local Postgres service refused connections. Result: 1 passed, 26 failed; failures reported `Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))`. |
| `cargo test -p djinn-slot --lib reply_loop_tests` | Built and ran the filtered lib tests, but failed because the local Postgres service refused connections. Result: 0 passed, 9 failed; failures reported `Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))`. |
| `cargo test -p djinn-agent --lib actors::slot` | Built and ran the focused agent slot filter, but failed because the local Postgres service refused connections. Result: 82 passed, 36 failed; failures reported `Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))`. |

Environment note: `DATABASE_URL` and `TEST_POSTGRES_URL` were set to
`postgres://postgres:postgres@127.0.0.1:5432/app_test?sslmode=disable`, but the
service at `127.0.0.1:5432` refused connections during the focused test runs.
No tests were disabled or skipped to work around this environment-only blocker.

---

## Task 2acc: Disabled-module grep sweep and assertion-retention verification

> Task: `019f1d88-0741-7b92-9d0b-77e462b7badc` — Verify no disabled slot test modules and preserve migrated assertions  
> Generated: 2026-07-01  
> Blocked-by: Task 6554 (canonical test-home map, above)

### Disabled/commented/ignored test sweep

A comprehensive grep/search sweep was performed from `server/` across both slot
trees (`crates/djinn-slot/src/` and `crates/djinn-agent/src/actors/slot/`) for
disabled, commented-out, or ignored slot behavior tests.

#### Search patterns and commands used

| # | Pattern | Command | Scope | Result |
|---|---|---|---|---|
| 1 | Commented-out `mod` declarations | `grep -rn '// *mod\b' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | **None found** |
| 2 | Commented-out `#[cfg(test)]` | `grep -rn '// *#\[cfg' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | **None found** |
| 3 | `#[ignore]` attribute | `grep -rn '#\[ignore\]' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | **None found** |
| 4 | TODO/FIXME disabled tests | `grep -rni 'TODO.*disabled\|FIXME.*disabled\|HACK.*disabled\|disabled.*test' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | **None found** |
| 5 | Commented-out `mod` declarations | `grep -rn '// *mod\b' crates/djinn-agent/src/actors/slot/ --include='*.rs'` | agent slot | **None found** |
| 6 | Commented-out `#[cfg(test)]` | `grep -rn '// *#\[cfg' crates/djinn-agent/src/actors/slot/ --include='*.rs'` | agent slot | **None found** |
| 7 | `#[ignore]` attribute | `grep -rn '#\[ignore\]' crates/djinn-agent/src/actors/slot/ --include='*.rs'` | agent slot | **None found** |
| 8 | TODO/FIXME disabled tests | `grep -rni 'TODO.*disabled\|FIXME.*disabled\|HACK.*disabled\|disabled.*test' crates/djinn-agent/src/actors/slot/ --include='*.rs'` | agent slot | **None found** |
| 9 | Old agent-only type references | `grep -rn 'AgentContext\|agent_context' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | `agent_context_from_db` is a **test helper** (returns `SlotContext`, naming artifact from extraction); `memory_enrichment.rs` has doc comments referencing `AgentContext` as migration context. No functional references to old types. |
| 10 | `djinn_agent::` imports | `grep -rn 'djinn_agent::' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | Only `lib.rs:21` (doc comment) and `host.rs:4` (doc comment). No code imports. |
| 11 | Commented-out test functions | `grep -rn '//.*#\[test\]\|//.*#\[tokio::test' crates/djinn-slot/src/ crates/djinn-agent/src/actors/slot/ --include='*.rs'` | Both | **None found** |
| 12 | `cfg(not(test))` excluding code | `grep -rn 'cfg.*not.*test' crates/djinn-slot/src/ --include='*.rs'` | djinn-slot | `reply_loop/budget.rs:94` — production-only guard on provider config fallback (not a disabled test). `pool/actor.rs:242` — production-only guard on `#[cfg(not(any(test, feature = "test-support")))]` (not a disabled test). |

#### Summary

**Zero disabled, commented-out, or ignored slot behavior test modules** exist in
either `djinn-slot` or `djinn-agent/src/actors/slot/`. All `#[cfg(test)]`
registrations are active. No test is disabled solely because of old agent-only
type references.

### Assertion-retention verification

All pre-existing behavior assertions are preserved in their canonical
`djinn-slot` homes:

| Canonical test file | Test count | Status |
|---|---|---|
| `djinn-slot/src/helpers_tests.rs` | 15 tests | All registered; 11 pass, 4 fail (DB ConnectionRefused) |
| `djinn-slot/src/llm_extraction_tests.rs` | 27 tests | All registered; 1 pass, 26 fail (DB ConnectionRefused) |
| `djinn-slot/src/reply_loop_tests.rs` | 9 tests | All registered; 0 pass, 9 fail (DB ConnectionRefused) |
| `djinn-slot/src/helpers/tests.rs` | 8 tests | All registered; 0 pass, 8 fail (DB ConnectionRefused) |
| `djinn-slot/src/pool/tests.rs` | 25 tests | All registered; 15 pass, 10 fail (DB ConnectionRefused) |
| `djinn-slot/src/reply_loop/tests.rs` | 28 tests | All registered; 7 pass, 21 fail (DB ConnectionRefused) |
| Inline: `reply_loop/budget.rs` | 4 tests | All pass |
| Inline: `reply_loop/error_handling.rs` | 3 tests | All pass |
| Inline: `reply_loop/loop_guard.rs` | 9 tests | All pass |
| Inline: `reply_loop/turn.rs` | 2 tests | All pass |
| Inline: `reply_loop/tool_dispatch.rs` | 3 tests | All pass |

**Total: 112 registered tests across 6 test files + 21 inline tests = 133 tests.**
No tests were removed, disabled, or skipped. All failures are environment-only
(Postgres `ConnectionRefused`).

**No slot behavior assertions were silently dropped.** The consolidation in Task
6554 removed the agent-side duplicate test files (`helpers_tests.rs` and
`llm_extraction_tests.rs`) but the canonical copies in `djinn-slot` retained all
assertions. No assertion removal was needed; therefore no removal entries are
recorded below.

### Intentionally removed assertions

*None.* No assertion was removed in this task or the blocking Task 6554 that was
not already documented as consolidated (moved from agent duplicate to canonical
djinn-slot home with identical assertion content).

### `djinn-slot` lib.rs test registration (current state)

```
// ─── Test modules ───────────────────────────────────────────────────────────

#[cfg(test)]
mod helpers_tests;           // 15 tests — slot helper behavior
#[cfg(test)]
mod llm_extraction_tests;    // 27 tests — LLM/session extraction behavior
#[cfg(test)]
mod reply_loop_tests;        //  9 tests — reply-loop smoke/integration
#[cfg(test)]
pub(crate) mod test_helpers; // shared test fixtures (no own tests)
```

No test module is commented out, cfg-disabled, or conditionally excluded.

### Module-local test registrations (current state)

All module-local `#[cfg(test)] mod tests;` declarations in djinn-slot are active:

| Module root | Registration | Status |
|---|---|---|
| `djinn-slot/src/helpers/mod.rs:57-59` | `#[cfg(test)] mod tests;` | Active (8 tests) |
| `djinn-slot/src/pool/mod.rs:10-11` | `#[cfg(test)] mod tests;` | Active (25 tests) |
| `djinn-slot/src/reply_loop/mod.rs:27-29` | `#[cfg(test)] mod tests;` | Active (28 tests) |
| `djinn-slot/src/reply_loop/budget.rs:376-377` | `#[cfg(test)] mod tests {` | Active (4 tests) |
| `djinn-slot/src/reply_loop/error_handling.rs:161-162` | `#[cfg(test)] mod tests {` | Active (3 tests) |
| `djinn-slot/src/reply_loop/loop_guard.rs:388-389` | `#[cfg(test)] mod tests {` | Active (9 tests) |
| `djinn-slot/src/reply_loop/turn.rs:1163-1164` | `#[cfg(test)] mod tests {` | Active (2 tests) |
| `djinn-slot/src/reply_loop/tool_dispatch.rs:484-485` | `#[cfg(test)] mod tests {` | Active (3 tests) |
| `djinn-slot/src/helpers/feedback.rs:207` | `#[cfg(test)] pub(crate) fn log_snippet` | Active (test helper, no own tests) |

### Task 2acc validation log

Commands were run from `server/`.

| Command | Outcome |
|---|---|
| `cargo fmt --package djinn-slot --package djinn-agent -- --check` | Passed; no formatter errors. |
| `cargo test -p djinn-slot --all-features --lib --no-run` | Compiled successfully; all test binaries produced. |
| `cargo test -p djinn-agent --all-features --lib --no-run` | Compiled successfully; all test binaries produced. |
| `cargo test -p djinn-slot --all-features --lib helpers_tests` | 11 passed, 4 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib reply_loop_tests` | 0 passed, 9 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib llm_extraction_tests` | 1 passed, 26 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib pool::tests` | 15 passed, 10 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib reply_loop::tests` | 7 passed, 21 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib helpers::tests` | 0 passed, 8 failed (DB ConnectionRefused). |
| `cargo test -p djinn-slot --all-features --lib reply_loop::budget::tests` | 4 passed, 0 failed. ✅ |
| `cargo test -p djinn-slot --all-features --lib reply_loop::error_handling::tests` | 3 passed, 0 failed. ✅ |
| `cargo test -p djinn-slot --all-features --lib reply_loop::loop_guard::tests` | 9 passed, 0 failed. ✅ |
| `cargo test -p djinn-slot --all-features --lib reply_loop::turn::tests` | 2 passed, 0 failed. ✅ |
| `cargo test -p djinn-slot --all-features --lib reply_loop::tool_dispatch::tests` | 3 passed, 0 failed. ✅ |
| `cargo test -p djinn-agent --all-features --lib actors::slot` | 82 passed, 36 failed (DB ConnectionRefused). |

**Non-DB tests all pass.** All DB-dependent failures report
`Sqlx(Io(Os { code: 111, kind: ConnectionRefused, message: "Connection refused" }))`.
No tests were disabled or skipped to work around this environment-only blocker.
