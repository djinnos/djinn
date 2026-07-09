# Learned-prompt backend cutover — final grep sweep (3x0w)

**Date:** 2026-07-09
**Task:** l9pd — Sweep stale learned-prompt backend tests, snapshots, fixtures, and grep evidence
**Landing after:** lv43, lb85, gton

## Grep commands run

```bash
# Core runtime/API symbols — expected zero Rust hits after cutover
grep -rn 'prompt_eval' --include='*.rs' --include='*.snap' server/
grep -rn 'append_learned_prompt' --include='*.rs' --include='*.snap' server/
grep -rn 'resolve_pending_amendment' --include='*.rs' --include='*.snap' server/
grep -rn 'PendingAmendmentEvaluation' --include='*.rs' --include='*.snap' server/
grep -rn 'WindowedRoleMetrics' --include='*.rs' --include='*.snap' server/
grep -rn 'learned_prompt_history' --include='*.rs' --include='*.snap' server/

# Broader text references — some intentional hits remain
grep -rn 'agent_amend_prompt' --include='*.rs' --include='*.snap' server/
grep -rn 'learned_prompt' --include='*.rs' --include='*.snap' server/
grep -rn 'learned-prompt' --include='*.rs' --include='*.snap' --include='*.json' server/
```

## Results after cleanup

### Zero-hit symbols (all clean)
- `prompt_eval` — 0 Rust hits
- `append_learned_prompt` — 0 Rust hits
- `resolve_pending_amendment` — 0 Rust hits
- `PendingAmendmentEvaluation` — 0 Rust hits
- `WindowedRoleMetrics` — 0 Rust hits
- `learned_prompt_history` — 0 Rust hits

### Remaining `agent_amend_prompt` hits — regression guards only
All remaining hits are negative assertions in test code (asserting the tool is NOT present):
- `djinn-agent/src/prompts/tests.rs`: `planner_prompt_omits_learned_prompt_amendment_guidance`, `architect_prompt_omits_learned_prompt_amendment_guidance_and_tool`, `tools_section_injected_into_rendered_prompt`
- These are valid regression guards — they prevent re-introduction of the removed tool.

### Remaining `learned_prompt` / `learned-prompt` hits — regression guards only
All remaining hits are negative assertions in test code:
- `djinn-agent/src/prompts/tests.rs`: function names and assertion messages referencing the removed feature (all `!prompt.contains(...)` assertions)
- `djinn-roles/src/prompts/tests.rs`: `planner_learned_prompt_guidance_absent_across_modes` — asserts the removed section does NOT appear

### Out-of-scope hits (documented)
- `server/docs/learned-prompt-harvest.md` — harvest artifact from epic t8p8
- `server/scripts/fixtures/learned-prompt-equivalence/` — harvest fixtures from epic t8p8
- SQL schema objects/migrations — owned by sibling epic 8m3c
- Generated/UI artifacts — owned by sibling epic 3sle

## Changes made

1. **`server/crates/djinn-roles/src/prompts/planner.md`** — Removed the entire "Learned-prompt amendments (agent-effectiveness grooming)" section (30 lines of stale guidance about `learned_prompt` and `agent_amend_prompt`).

2. **`server/crates/djinn-agent/src/prompts/tests.rs`** — Converted `planner_prompt_contains_learned_prompt_amendment_guidance` (100 lines of positive assertions) to `planner_prompt_omits_learned_prompt_amendment_guidance` (regression guard asserting the section is NOT present).

3. **`server/crates/djinn-roles/src/prompts/tests.rs`** — Converted `planner_learned_prompt_guidance_present_across_modes` to `planner_learned_prompt_guidance_absent_across_modes` (regression guard asserting the section does NOT appear in any Planner mode).

4. **`server/crates/djinn-control-plane/src/tools/agent_tools.rs`** — Removed stale `learned_prompt is machine-managed` doc comment and tool description string from `agent_update`.

5. **`server/crates/djinn-mcp-extension/src/tool_defs.rs`** — Updated stale comment about `learned_prompts` to generic text.

6. **`server/crates/djinn-provider/tests/fixtures/tool_schema_projection/builtin/djinn_mcp_server.json`** — Updated `agent_update` tool description to match the updated source (removed learned_prompt mention).

## Verification

- `cargo clippy -p djinn-roles` — clean
- `cargo clippy -p djinn-agent` — clean
- `cargo clippy -p djinn-control-plane` — clean
- `cargo clippy -p djinn-mcp-extension` — clean
- `cargo test -p djinn-agent --lib -- prompts::tests` — 40 tests pass
- `cargo test -p djinn-roles --lib -- prompts::tests` — 59 tests pass
- `cargo fmt` — applied, no changes needed
