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
