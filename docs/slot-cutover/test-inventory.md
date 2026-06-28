# Slot Cut-Over Test Inventory

> Epic: `aaiz` — Slot cut-over foundation: baseline inventory and reconciliation plan  
> Proposal: `flpe` — Finish the djinn-slot extraction cut-over (de-duplicate the slot subsystem)  
> Artifact: `docs/slot-cutover/test-inventory.md`  
> Scope: documentation-only; no behavior, API, visibility, test registration, or assertion changes.

---

## 1. Duplicated / disabled test files overview

Both slot trees contain near-identical copies of the same test modules. The table below summarizes every duplicated or disabled test file, its line count, test count, and current status.

| File | Agent path | Slot path | Agent lines | Slot lines | Agent tests | Slot tests | Status |
|------|------------|-----------|-------------|------------|-------------|------------|--------|
| `llm_extraction_tests.rs` | `server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs` | `server/crates/djinn-slot/src/llm_extraction_tests.rs` | 2,046 | 2,046 | 1 | 1 | **Enabled in both** |
| `helpers_tests.rs` | `server/crates/djinn-agent/src/actors/slot/helpers_tests.rs` | `server/crates/djinn-slot/src/helpers_tests.rs` | 487 | 487 | 15 | 15 | **Enabled in both** |
| `reply_loop_tests.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop_tests.rs` | `server/crates/djinn-slot/src/reply_loop_tests.rs` | 532 | 532 | 9 | 9 | **Enabled in agent; commented out in slot** |
| `helpers/tests.rs` | `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` | `server/crates/djinn-slot/src/helpers/tests.rs` | 894 | 899 | 8 | 8 | **Enabled in both** |
| `reply_loop/tests.rs` | `server/crates/djinn-agent/src/actors/slot/reply_loop/tests.rs` | `server/crates/djinn-slot/src/reply_loop/tests.rs` | 2,410 | 2,410 | 28 | 28 | **Enabled in both** |
| `pool/tests.rs` | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs` | `server/crates/djinn-slot/src/pool/tests.rs` | 2,157 | 2,157 | 2 | 2 | **Enabled in both** |

**Total duplicated test lines:** 17,057 (agent) + 17,061 (slot) = **34,118 lines** of near-identical test code. This is a major contributor to the combined 45,312-line baseline.

---

## 2. Per-file inventory

### 2.1 `llm_extraction_tests.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/llm_extraction_tests.rs` |
| **Lines** | 2,046 each |
| **Tests** | 1 `#[test]` each (plus many helper fns) |
| **Current status** | Enabled in both trees |
| **Likely canonical home** | `djinn-slot/src/llm_extraction_tests.rs` |

#### Assertion / behavior inventory

- `structural_extraction_produces_correct_taxonomy` — end-to-end test of `run_structural_extraction` using a `FakeProvider` scripted with a JSON taxonomy response.
- Helper functions: `make_tmpdir`, `semantic_duplicate_candidate_lookup`, `fake_extraction_provider`, `novelty_candidate`, `novelty_failure_candidate_lookup`, `anchor_extraction_provider`, `complete_case_body`, `case_body_missing_reusable_lesson`, `low_paragraph_pitfall_body`.
- The test exercises `llm_extraction::run_structural_extraction`, provider building, candidate deduplication, and taxonomy parsing.

#### Fixture / API blockers

- Both copies import `crate::actors::slot::llm_extraction` (agent) vs `crate::llm_extraction` (slot). The test body is byte-for-byte identical after the module-path prefix.
- The agent copy compiles because `llm_extraction.rs` is still present in the agent tree. The slot copy compiles because `llm_extraction.rs` is present in the slot tree.
- **Blocker:** whichever tree is chosen as canonical, the other tree's `llm_extraction.rs` must still exist (or be re-exported) so the test's `use super::llm_extraction` resolves.

#### Future removal justification

- The agent copy can be deleted once the agent's `llm_extraction.rs` is removed and all agent-side callers import from `djinn_slot::llm_extraction`. The test itself does not reference `AgentContext` directly; it only needs the module path to resolve.
- **Not** compatibility glue — it is a canonical behavior test.

#### Downstream validation commands

```bash
cargo test -p djinn-agent --lib llm_extraction_tests
cargo test -p djinn-slot --lib llm_extraction_tests
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/llm_extraction_tests.rs \
  server/crates/djinn-slot/src/llm_extraction_tests.rs
```

---

### 2.2 `helpers_tests.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/helpers_tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/helpers_tests.rs` |
| **Lines** | 487 each |
| **Tests** | 15 (11 `#[test]`, 4 `#[tokio::test]`) |
| **Current status** | Enabled in both trees |
| **Likely canonical home** | `djinn-slot/src/helpers_tests.rs` |

#### Assertion / behavior inventory

| Test name | Type | Behavior covered |
|-----------|------|------------------|
| `parse_conflict_metadata_patterns` | `#[test]` | JSON prefix parsing for merge-conflict metadata |
| `provider_helpers_cover_branches` | `#[test]` | `format_family_for_provider`, `capabilities_for_provider`, `auth_method_for_provider`, `default_base_url` branch coverage (Anthropic, Google, OpenAI, OpenAIResponses, Fireworks, Xiaomi, Kimi, synthetic, local) |
| `parse_model_id_valid_and_invalid` | `#[test]` | `parse_model_id` split on `/` |
| `text_helpers_cover_limits_and_empty` | `#[test]` | `log_snippet` truncation and empty handling |
| `command_formatters` | `#[test]` | `format_command_details` markdown formatting |
| `recent_feedback_filters_orders_and_limits` | `#[tokio::test]` | `recent_feedback` actor-role filtering, ordering, cap |
| `initial_user_message_default_and_feedback` | `#[tokio::test]` | `initial_user_message_for_task` default vs. PM feedback |
| `latest_ci_feedback_respects_cycle_floor` | `#[test]` | `raw_ci_feedback_in_cycle` timestamp floor logic |
| `initial_user_message_combines_reviewer_and_ci_feedback` | `#[tokio::test]` | Combined reviewer + CI feedback message generation |
| `initial_user_message_reviewer_only_preserves_behavior` | `#[tokio::test]` | Reviewer-only message path |
| `combined_budget_small_sections_pass_through_untouched` | `#[test]` | `budget_combined_sections` no-op for small inputs |
| `combined_budget_oversized_reviewer_does_not_starve_ci` | `#[test]` | Reviewer truncation does not starve CI section |
| `combined_budget_oversized_ci_does_not_starve_reviewer` | `#[test]` | CI truncation does not starve reviewer section |
| `combined_budget_lends_unused_room_when_both_large` | `#[test]` | Shared pool distribution when both sections exceed floor |
| `combined_budget_single_section_gets_more_than_floor` | `#[test]` | Single-section borrowing of empty peer's share |

#### Fixture / API blockers

- Both copies use `crate::test_helpers::{agent_context_from_db, create_test_db, …}` (agent) vs `crate::test_helpers::{…}` (slot). The helper APIs are identical.
- The `initial_user_message_for_task` tests require a live SQLite test DB and `TaskRepository`.
- **Blocker:** `test_helpers` module must remain in whichever tree hosts the canonical tests. The slot tree already has `test_helpers.rs` (public `pub(crate)`); the agent tree has its own `test_helpers` module outside the slot directory. Consolidation of `test_helpers` is a prerequisite.

#### Future removal justification

- The agent copy can be deleted once agent-side callers of `helpers::*` are proven to compile against `djinn_slot::helpers::*` (or re-exports). The tests do not exercise agent-specific behavior.
- **Not** compatibility glue — canonical behavior tests.

#### Downstream validation commands

```bash
cargo test -p djinn-agent --lib helpers_tests
cargo test -p djinn-slot --lib helpers_tests
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/helpers_tests.rs \
  server/crates/djinn-slot/src/helpers_tests.rs
```

---

### 2.3 `reply_loop_tests.rs`

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/reply_loop_tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/reply_loop_tests.rs` |
| **Lines** | 532 each |
| **Tests** | 9 (all `#[tokio::test]`) |
| **Current status** | **Enabled in agent; commented out in slot** (`lib.rs` lines 56–62) |
| **Likely canonical home** | `djinn-slot/src/reply_loop_tests.rs` (after re-enable) |

#### Assertion / behavior inventory

| Test name | Behavior covered |
|-----------|------------------|
| `text_only_completion_path_ends_without_nudge_when_no_tools_exist` | Text-only LLM response ends loop without nudge when no tools registered |
| `tool_call_execution_adds_tool_result_and_continues_to_next_turn` | Tool call → tool result message → next provider turn |
| `finalize_tool_detection_ends_loop_without_extra_provider_turn` | `submit_work` tool call ends loop immediately; no extra turn |
| `empty_response_retries_then_injects_nudge_into_second_turn_history` | Empty provider response triggers retry + nudge message injection |
| `max_nudge_abort_returns_clean_error_path` | Three consecutive text-only responses abort with clean error |
| `provider_error_propagates_from_shared_failing_provider` | Provider `Err` propagates through `run_reply_loop` |
| `metadata_drives_streaming_dispatch_for_safe_tools` | `concurrent_safe=true` metadata drives safe dispatch |
| `missing_metadata_defaults_to_unsafe_dispatch` | Absence of safety metadata defaults to unsafe dispatch |
| `side_query_tools_share_normal_tool_result_turn_and_keep_order` | Mixed safe + unsafe tools in same turn preserve order |

#### Fixture / API blockers

- **Agent copy** compiles because it uses:
  - `crate::context::AgentContext` (available in agent)
  - `crate::test_helpers::{FailingProvider, FakeProvider, …}` (available in agent)
  - `crate::output_parser::ParsedAgentOutput` (available in agent)
- **Slot copy** is commented out because it references:
  - `crate::context::AgentContext` — **does not exist in `djinn-slot`**
  - `crate::test_helpers::test_services` — **does not exist in `djinn-slot`**
  - The old `ReplyLoopContext` struct with many fields removed during extraction
- **Blocker for re-enable:**
  1. Replace `AgentContext` with `SlotContext` in the slot copy's `make_context` and `run_with_provider`.
  2. Provide `test_services` equivalent in `djinn-slot/src/test_helpers.rs` (or adapt the test to use `SlotHostCallbacks` mock).
  3. Ensure `ReplyLoopContext` fields in `djinn-slot::reply_loop` match what the test expects.

#### Future removal justification

- The agent copy is **compatibility glue** — it keeps the reply-loop tests running while the canonical implementation is still split. Once the slot copy is re-enabled and passing, the agent copy should be deleted.
- The commented-out slot copy is the **canonical target**; the comment in `lib.rs` explicitly says "Re-enable after the reply loop is fully extracted to djinn-slot."

#### Downstream validation commands

```bash
# Agent side (currently passing)
cargo test -p djinn-agent --lib reply_loop_tests

# Slot side (currently disabled; will fail to compile if uncommented)
# cargo test -p djinn-slot --lib reply_loop_tests

# Diff (byte-for-byte identical except module path prefix)
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/reply_loop_tests.rs \
  server/crates/djinn-slot/src/reply_loop_tests.rs
```

---

### 2.4 `helpers/tests.rs` (module-local tests)

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/helpers/tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/helpers/tests.rs` |
| **Lines** | 894 (agent) / 899 (slot) |
| **Tests** | 8 (1 `#[test]`, 7 `#[tokio::test]`) |
| **Current status** | Enabled in both trees |
| **Likely canonical home** | `djinn-slot/src/helpers/tests.rs` |

#### Assertion / behavior inventory

| Test name | Behavior covered |
|-----------|------------------|
| `auto_code_context_role_flag_parses_csv` | `CODE_CONTEXT_ROLES` env var CSV parsing |
| `code_context_for_worker_task` | `build_code_context` for worker role (symbol impact, changed files, imports) |
| `code_context_for_architect_task` | `build_code_context` for architect role (no symbol impact, full file list) |
| `code_context_for_reviewer_task` | `build_code_context` for reviewer role (PR diff, no symbol impact) |
| `code_context_with_touched_symbols` | Symbol-impact filtering via `touched_symbols` |
| `code_context_with_impact_entries` | `ImpactEntry` direct injection |
| `code_context_with_changed_files` | Changed-file list injection |
| `code_context_with_imports` | Import-graph injection |

#### Fixture / API blockers

- Heavy use of `crate::test_helpers` (DB setup, project/task creation, `agent_context_from_db`).
- `build_code_context` requires `SlotContext` (or `AgentContext` in agent copy) and `Task`.
- **Blocker:** same as `helpers_tests.rs` — `test_helpers` consolidation.

#### Future removal justification

- **Not** compatibility glue — canonical behavior tests for `build_code_context`.

#### Downstream validation commands

```bash
cargo test -p djinn-agent --lib helpers::tests
cargo test -p djinn-slot --lib helpers::tests
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/helpers/tests.rs \
  server/crates/djinn-slot/src/helpers/tests.rs
```

---

### 2.5 `reply_loop/tests.rs` (module-local tests)

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/reply_loop/tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/reply_loop/tests.rs` |
| **Lines** | 2,410 each |
| **Tests** | 28 (11 `#[test]`, 17 `#[tokio::test]`) |
| **Current status** | Enabled in both trees |
| **Likely canonical home** | `djinn-slot/src/reply_loop/tests.rs` |

#### Assertion / behavior inventory (named checklist)

| # | Test name | Behavior covered |
|---|-----------|------------------|
| 1 | `extract_stash_content_shell_extracts_stdout` | `extract_stash_content` parses shell stdout |
| 2 | `extract_stash_content_shell_includes_stderr_and_exit_code` | Shell stderr + exit code preserved |
| 3 | `extract_stash_content_non_shell_returns_none` | Non-shell stash returns `None` |
| 4 | `budget_tracking_accurate_for_text_and_tool_calls` | Token budget tracking across text + tool turns |
| 5 | `budget_exceeded_soft_limit_triggers_wind_down` | Soft-limit breach triggers wind-down |
| 6 | `budget_exceeded_hard_limit_aborts_loop` | Hard-limit breach aborts loop |
| 7 | `serialize_llm_input_preserves_system_tools_and_full_history_order` | `serialize_llm_input` ordering |
| 8 | `serialize_llm_input_preserves_parallel_tool_call_order` | Parallel tool call ordering in serialization |
| 9 | `turn_with_text_response_updates_conversation_and_budget` | Text turn updates conversation + budget |
| 10 | `turn_with_tool_call_updates_conversation_and_budget` | Tool turn updates conversation + budget |
| 11 | `turn_with_tool_call_and_error_updates_conversation` | Error tool result turn handling |
| 12 | `turn_with_finalize_tool_updates_conversation` | Finalize tool turn ends without result |
| 13 | `turn_with_empty_response_retries_and_nudges` | Empty response retry + nudge |
| 14 | `turn_with_max_nudge_exhaustion_aborts` | Nudge exhaustion abort |
| 15 | `turn_with_provider_error_propagates` | Provider error propagation |
| 16 | `turn_with_streaming_safe_tools_allows_parallel_dispatch` | Streaming safe-tool parallel dispatch |
| 17 | `turn_with_streaming_unsafe_tools_blocks_parallel_dispatch` | Streaming unsafe-tool blocking |
| 18 | `turn_with_mixed_safety_tools_dispatches_correctly` | Mixed safety dispatch |
| 19 | `turn_with_missing_metadata_defaults_unsafe` | Missing metadata → unsafe |
| 20 | `loop_guard_enforces_max_turns` | `LoopGuard` max-turn enforcement |
| 21 | `loop_guard_tracks_consecutive_text_responses` | Consecutive text tracking |
| 22 | `persistence_saves_and_loads_conversation` | Conversation persistence round-trip |
| 23 | `persistence_handles_missing_file_gracefully` | Missing persistence file handling |
| 24 | `error_handling_classifies_provider_errors` | Provider error classification |
| 25 | `error_handling_classifies_tool_errors` | Tool error classification |
| 26 | `error_handling_classifies_budget_errors` | Budget error classification |
| 27 | `budget_env_overrides_default_limits` | `SESSION_BUDGET_*` env override |
| 28 | `budget_env_hard_limit_zero_means_unlimited` | Hard limit `0` → unlimited |

#### Fixture / API blockers

- Uses `crate::test_helpers` (DB, fake providers, context builders).
- Some tests reference `AgentContext` (agent) vs `SlotContext` (slot) through `reply_loop/mod.rs` imports.
- **Blocker:** `test_helpers` consolidation and context-type unification.

#### Future removal justification

- **Not** compatibility glue — canonical behavior tests for `reply_loop` submodules (turn, budget, persistence, error handling, loop guard, streaming dispatch).

#### Downstream validation commands

```bash
cargo test -p djinn-agent --lib reply_loop::tests
cargo test -p djinn-slot --lib reply_loop::tests
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/reply_loop/tests.rs \
  server/crates/djinn-slot/src/reply_loop/tests.rs
```

---

### 2.6 `pool/tests.rs` (module-local tests)

| | |
|---|---|
| **Agent path** | `server/crates/djinn-agent/src/actors/slot/pool/tests.rs` |
| **Slot path** | `server/crates/djinn-slot/src/pool/tests.rs` |
| **Lines** | 2,157 each |
| **Tests** | 2 (both `#[tokio::test]`) |
| **Current status** | Enabled in both trees |
| **Likely canonical home** | `djinn-slot/src/pool/tests.rs` |

#### Assertion / behavior inventory

| Test name | Behavior covered |
|-----------|------------------|
| `pool_spawns_slots_and_routes_commands` | `SlotPool` spawn, `SlotCommand` routing, `SlotEvent::Free` / `Killed` delivery |
| `pool_reclaims_slot_on_task_completion` | Slot reclamation after task finish, invariant checks |

#### Fixture / API blockers

- Uses `test_app_state`, `test_slot_factory`, `blocking_cancel_slot_factory`, `new_white_box_pool`, `inject_stale_busy_free_slot`, `assert_slot_pool_invariants_after`.
- Requires `SlotContext` (or `AgentContext`) and temporary directories.
- **Blocker:** `test_helpers` consolidation; `SlotHandle::spawn` signature must match between trees.

#### Future removal justification

- **Not** compatibility glue — canonical behavior tests for the slot pool actor.

#### Downstream validation commands

```bash
cargo test -p djinn-agent --lib pool::tests
cargo test -p djinn-slot --lib pool::tests
git diff --no-index \
  server/crates/djinn-agent/src/actors/slot/pool/tests.rs \
  server/crates/djinn-slot/src/pool/tests.rs
```

---

## 3. Commented-out `reply_loop_tests` registration in `djinn-slot/src/lib.rs`

### Current state

From `server/crates/djinn-slot/src/lib.rs` lines 56–62:

```rust
// reply_loop_tests.rs: disabled — tests reference `crate::context::AgentContext`,
// the old ReplyLoopContext struct (with many fields removed during extraction),
// and `crate::test_helpers::test_services` which no longer exists.
// These tests exercise the full reply loop implementation which is still owned
// by djinn-agent. Re-enable after the reply loop is fully extracted to djinn-slot.
// #[cfg(test)]
// mod reply_loop_tests;
```

### Why it is disabled

1. **Context type mismatch:** The test file uses `crate::context::AgentContext`, which does not exist in `djinn-slot`. It must be rewritten to use `crate::host::SlotContext`.
2. **Missing `test_services`:** The test calls `crate::test_helpers::test_services()`, which is not present in `djinn-slot/src/test_helpers.rs`.
3. **ReplyLoopContext drift:** The `ReplyLoopContext` struct in `djinn-slot::reply_loop` may have fewer fields than the agent copy's version (fields removed during extraction).

### What must happen before re-enable

| Step | Action | Owner (proposed) |
|------|--------|------------------|
| 1 | Ensure `ReplyLoopContext` in `djinn-slot::reply_loop` has all fields needed by the test (or adapt the test) | Cut-over implementation epic (`lvft`) |
| 2 | Add `test_services()` (or equivalent mock) to `djinn-slot/src/test_helpers.rs` | Cut-over implementation epic (`lvft`) |
| 3 | Rewrite `make_context` in `reply_loop_tests.rs` to use `SlotContext` | Cut-over implementation epic (`lvft`) |
| 4 | Uncomment `mod reply_loop_tests;` in `lib.rs` | Cut-over implementation epic (`lvft`) |
| 5 | Run `cargo test -p djinn-slot --lib reply_loop_tests` and confirm pass | Verification epic (`0ecv`) |
| 6 | Delete agent copy of `reply_loop_tests.rs` | Host facade epic (`p6i4`) |

---

## 4. Cross-cutting concerns

### 4.1 `test_helpers` consolidation

Both trees depend on a `test_helpers` module that provides:
- `create_test_db`, `create_test_project`, `create_test_epic`, `create_test_task`
- `agent_context_from_db` (or `slot_context_from_db`)
- `FakeProvider`, `FailingProvider`
- `test_services()` (agent only)
- `test_path`

The slot tree already has `djinn-slot/src/test_helpers.rs` (public `pub(crate)`). The agent tree has `djinn-agent/src/test_helpers.rs` (outside the slot directory). Many duplicated tests import from `crate::test_helpers`. Before deleting either copy of a test file, confirm that the canonical `test_helpers` has all required symbols.

### 4.2 Context-type unification in tests

Tests that construct a context (e.g., `make_context`, `test_app_state`) currently use `AgentContext` in the agent tree and `SlotContext` in the slot tree. The canonical tests should use `SlotContext`. Any agent-specific adapter logic should be tested separately (if at all) and not duplicated in every test module.

### 4.3 `output_parser::ParsedAgentOutput`

`reply_loop_tests.rs` returns `crate::output_parser::ParsedAgentOutput`. In `djinn-slot`, `output_parser.rs` exists and is a public module. In `djinn-agent`, `output_parser` lives outside the slot tree. The test's return type must resolve in whichever crate hosts the canonical test.

### 4.4 Diff identity

For every duplicated test file listed above, `git diff --no-index` shows either:
- **Zero diff** (e.g., `helpers_tests.rs`, `reply_loop_tests.rs`, `llm_extraction_tests.rs`), or
- **Near-zero diff** limited to `crate::actors::slot::…` → `crate::…` path rewrites (e.g., `helpers/tests.rs`, `reply_loop/tests.rs`, `pool/tests.rs`).

This confirms that the tests are **true duplicates** — they exercise the same behavior against the same APIs, differing only in module path prefixes.

---

## 5. Summary checklist for downstream tasks

- [ ] **Re-enable `reply_loop_tests` in `djinn-slot`** — unblock by fixing `AgentContext` → `SlotContext`, adding `test_services`, and aligning `ReplyLoopContext`.
- [ ] **Consolidate `test_helpers`** — ensure canonical `djinn-slot/src/test_helpers.rs` has all symbols needed by the tests.
- [ ] **Delete agent `llm_extraction_tests.rs`** once `llm_extraction.rs` canonical source is in `djinn-slot` and agent compiles against re-exports.
- [ ] **Delete agent `helpers_tests.rs`** once `helpers.rs` canonical source is in `djinn-slot` and agent compiles against re-exports.
- [ ] **Delete agent `reply_loop_tests.rs`** once slot copy is re-enabled and passing.
- [ ] **Delete agent `helpers/tests.rs`** once `helpers/` canonical source is in `djinn-slot`.
- [ ] **Delete agent `reply_loop/tests.rs`** once `reply_loop/` canonical source is in `djinn-slot`.
- [ ] **Delete agent `pool/tests.rs`** once `pool/` canonical source is in `djinn-slot`.
- [ ] **Verify line-count reduction** — deleting all six agent test files removes ~8,526 lines (agent test total). Deleting all six slot test files (if agent is chosen as canonical) removes ~8,531 lines. The cut-over must pick one side and delete the other.

---

*Generated by task `yz2j` as part of epic `aaiz` — Slot cut-over foundation.*
