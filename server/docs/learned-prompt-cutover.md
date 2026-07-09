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
- `server/scripts/learned-prompt-inventory.sql`, `learned-prompt-inventory.sh`, `learned-prompt-equivalence.sh` — harvest tooling from t8p8
- SQL schema objects/migrations — owned by sibling epic 8m3c
- ADR design records (`.djinn/decisions/adr-038-*.md`, `adr-051-*.md`) — immutable historical records
- Generated/UI artifacts — swept by sibling epic 3sle (see below)

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

---

## Final sweep — 3sle frontend/generated/UI artifacts

**Date:** 2026-07-09
**Task:** 3b8u (3sle) — Sweep learned-prompt generated artifacts, route fixtures, and final grep evidence
**Landing after:** nqcn (UI/API client removal), xij7 (backend MCP schema snapshot refresh), kutw (frontend MCP tool type regeneration)

### Grep commands run (repo-wide, all source/generated/types)

```bash
grep -rn 'learned_prompt' --include='*.rs' --include='*.ts' --include='*.tsx' --include='*.json' --include='*.sql' --include='*.toml' --include='*.yaml' --include='*.yml' .
grep -rn 'learned-prompt' --include='*.rs' --include='*.ts' --include='*.tsx' --include='*.json' --include='*.sql' --include='*.toml' --include='*.yaml' --include='*.yml' .
grep -rn 'learned_prompt_history' --include='*.rs' --include='*.ts' --include='*.tsx' --include='*.json' --include='*.sql' --include='*.toml' --include='*.yaml' --include='*.yml' .
grep -rn 'agent_amend_prompt' --include='*.rs' --include='*.ts' --include='*.tsx' --include='*.json' --include='*.sql' --include='*.toml' --include='*.yaml' --include='*.yml' .
grep -rn 'learned_prompt\|learned-prompt\|learned_prompt_history\|agent_amend_prompt' --include='*.snap' .
grep -rn 'learned_prompt\|learned-prompt\|learned_prompt_history\|agent_amend_prompt' --include='*.md' .
grep -rn 'learned_prompt\|learned-prompt\|learned_prompt_history\|agent_amend_prompt' --include='*.html' --include='*.css' .
```

### UI/generated/test artifact scope — zero hits

| Area | Status |
|---|---|
| `ui/src/**` (all `.ts`, `.tsx`) | **Clean** — no learned-prompt references remain |
| `ui/src/api/generated/mcp-tools.gen.ts` | **Clean** — `agent_amend_prompt` absent; `agent_metrics` present |
| `server/.sqlx/query-*.json` | **Clean** — no learned-prompt query metadata |
| `server/crates/djinn-graph/src/route_extraction/tests/fixtures/` | **Clean** — no learned-prompt route fixtures |
| `server/crates/djinn-agent/src/extension/tests/snapshots/` | **Clean** — no `agent_amend_prompt` in MCP snapshots |
| `server/crates/djinn-mcp-extension/src/tests/snapshots/` | **Clean** — no `agent_amend_prompt` in MCP snapshots |
| `server/crates/djinn-provider/tests/fixtures/` | **Clean** — no learned-prompt projection fixtures |
| `*.snap` files (any crate) | **Clean** — zero snapshot hits |

### Remaining repository-wide hits — all intentional

| Category | Files | Rationale |
|---|---|---|
| Negative regression guards | `djinn-agent/src/prompts/tests.rs`, `djinn-roles/src/prompts/tests.rs` | Assert removed guidance/tools do NOT reappear |
| Harvest artifacts (t8p8) | `server/docs/learned-prompt-harvest.md`, `server/scripts/learned-prompt-*.{sql,sh}`, `server/scripts/fixtures/learned-prompt-equivalence/` | Preserved prompt-equivalence evidence from pre-removal harvest |
| Migration/schema (8m3c) | `server/crates/djinn-db/migrations_postgres/1_initial_schema.sql` | Initial schema migration; schema drop owned by sibling epic 8m3c |
| Cutover documentation | `server/docs/learned-prompt-cutover.md` (this file) | Explicit cutover record |
| ADR design records | `.djinn/decisions/adr-038-*.md`, `.djinn/decisions/adr-051-*.md` | Historical design decisions (immutable records) |

### No code changes required

The sibling tasks nqcn, xij7, and kutw fully cleaned all UI/generated/test artifacts. This sweep confirms the final state and records the evidence above. Only this documentation update was made.

---

## Proposal supersession — design record annotations (this task)

**Date:** 2026-07-09
**Task:** 1hqf — Annotate ADRs and proposal supersession documentation for learned-prompt removal

### Proposals superseded by z5f9

Proposal **z5f9** ("Remove the learned-prompt auto-coaching subsystem: hand-author specialist role prompts, drop the per-project self-tuning engine") supersedes the following proposals:

| Proposal | Title | Why superseded |
|---|---|---|
| **2fd1** | "The Trial Room: a live cockpit for coaching your AI colleagues — watch each prompt experiment gather evidence and overrule the machine's verdict" | Proposed a UI cockpit over the `learned_prompt_history` eval engine (`prompt_eval.rs`, `resolve_pending_amendment`, `agent_metrics` decision math). The entire engine and its database tables were removed by z5f9; the Trial Room UI has no substrate to operate on. |
| **dsa0** | "Make per-project default-role prompts editable in the UI (and revive/remove the dormant learned_prompt loop)" | Shipped `LearnedPromptSection.tsx` rendering settled `learned_prompt_history` rows and editable default-role prompts. The `learned_prompt_history` table and `learned_prompt` column were dropped by z5f9; the LearnedPromptSection UI was removed by epic 3sle. The editable-default-role-prompts portion (hand-authored `system_prompt_extensions`) was preserved independently. |

### ADR annotations applied

| ADR | What was annotated |
|---|---|
| **ADR-038** (`.djinn/decisions/adr-038-*.md`) | Added top-level supersession blockquote marking Phase 38d, `learned_prompt`, and `agent_amend_prompt` as removed by z5f9. Phase 38d bullet list struck through with explanatory blockquote. |
| **ADR-051** (both variants in `.djinn/decisions/`) | Added top-level historical note blockquote explaining `agent_amend_prompt` was removed by z5f9. Inline `agent_amend_prompt` references on migration step 4 and test-update lines struck through with `> z5f9 note:` annotations. |

### Worker-accessible metadata path

Proposals 2fd1 and dsa0 are stored as proposal entities in the project memory system (Dolt-backed). Worker sessions can search proposals via `memory_search(entity_types=["proposal"])` but **cannot read or edit proposal metadata** — `memory_read` and `memory_edit` return "note not found" for proposal permalinks. The supersession metadata action (setting `superseded_by: z5f9` on proposals 2fd1 and dsa0) remains **planner/operator-only** and must be performed through the planner's proposal-management surface or a direct Dolt commit.

This documentation record is the worker-accessible supersession evidence. The file is referenced by the cutover sweep table above and by the ADR annotations.
