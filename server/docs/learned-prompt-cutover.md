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

---

## Final cutover grep evidence and invariants (8m3c / dfgt)

**Date:** 2026-07-09
**Task:** dfgt — Record final learned-prompt cutover grep evidence and invariants
**Landing after:** 5wyg (drop migration PR #1814), 1hqf (ADR annotations PR #1813)

### 1. Repository-wide grep evidence

All greps run from repository root with `git grep -n` against the full tracked-tree.

#### `learned_prompt` (underscore form)

| File | Line(s) | Classification |
|---|---|---|
| `.djinn/decisions/adr-038-*.md` | 16, 145, 240, 247 | **Superseded/removal documentation** — ADR-038 annotated by 1hqf with z5f9 blockquote |
| `scripts/test-learned-prompt-harvest-contract.sh` | 30, 129, 137 | **Harvest artifact/tooling** (t8p8) — contract validation for the pre-removal harvest |
| `server/crates/djinn-agent/src/prompts/tests.rs` | 258, 277 | **Negative regression guards** — `planner_prompt_omits_learned_prompt_amendment_guidance`, `architect_prompt_omits_learned_prompt_amendment_guidance_and_tool` |
| `server/crates/djinn-db/migrations_postgres/1_initial_schema.sql` | 457, 473, 474, 482, 485 | **Initial schema migration** (immutable applied migration; CI guard prevents modification). Correctly retained per sqlx migration rules — the new migration `101_drop_learned_prompt_schema.sql` drops these objects. |
| `server/crates/djinn-roles/src/prompts/tests.rs` | 656 | **Negative regression guard** — `planner_learned_prompt_guidance_absent_across_modes` |
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** |
| `server/docs/learned-prompt-harvest.md` | 18, 21, 25, 73, 77, 92, 97, 98, 102, 136, 195, 198, 228, 251, 252, 260, 262, 268, 406, 442, 444, 480 | **Harvest artifact** (t8p8) — pre-removal runbook preserved as audit evidence |
| `server/scripts/fixtures/learned-prompt-equivalence/README.md` | 17, 18, 21, 23, 40 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-equivalence.sh` | 24, 30, 31, 33, 35, 40, 79, 80, 86, 88 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-inventory.sql` | 5, 28 | **Harvest artifact/tooling** (t8p8) |

#### `learned-prompt` (hyphen form)

| File | Line(s) | Classification |
|---|---|---|
| `.djinn/decisions/adr-038-*.md` | 18 | **Superseded/removal documentation** |
| `.djinn/decisions/adr-051-*.md` (both variants) | 22 | **Superseded/removal documentation** |
| `scripts/test-learned-prompt-harvest-contract.sh` | 2, 13, 20, 28, 34, 100, 107, 125, 148, 153 | **Harvest artifact/tooling** (t8p8) |
| `server/crates/djinn-agent/src/prompts/tests.rs` | 255, 266, 274, 285 | **Negative regression guards** |
| `server/crates/djinn-roles/src/prompts/tests.rs` | 653, 665 | **Negative regression guards** |
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** |
| `server/docs/learned-prompt-harvest.md` | 4, 8, 14, 17, 67, 124, 125, 146, 161, 277, 282, 356, 360, 378, 421, 446, 450, 451, 454, 456, 460, 469, 474, 478, 482 | **Harvest artifact** (t8p8) |
| `server/scripts/fixtures/learned-prompt-equivalence/README.md` | 4, 6 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-equivalence.sh` | 3, 4, 5, 92, 99, 126, 141, 154, 264, 279, 288 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-inventory.sh` | 3, 4, 7, 24, 28, 36, 39, 42, 47, 52, 53, 64, 65, 186, 191, 192 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-inventory.sql` | 1, 10, 18 | **Harvest artifact/tooling** (t8p8) |

#### `learned_prompt_history`

| File | Line(s) | Classification |
|---|---|---|
| `.djinn/decisions/adr-038-*.md` | 240 | **Superseded/removal documentation** |
| `scripts/test-learned-prompt-harvest-contract.sh` | 30, 129, 137 | **Harvest artifact/tooling** (t8p8) |
| `server/crates/djinn-db/migrations_postgres/1_initial_schema.sql` | 473, 474, 482, 485 | **Initial schema migration** (immutable) — dropped by migration 101 |
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** |
| `server/docs/learned-prompt-harvest.md` | 18, 21, 73, 92, 97, 228, 406, 444, 480 | **Harvest artifact** (t8p8) |
| `server/scripts/learned-prompt-equivalence.sh` | 40 | **Harvest artifact/tooling** (t8p8) |
| `server/scripts/learned-prompt-inventory.sql` | 28 | **Harvest artifact/tooling** (t8p8) |

#### `prompt_eval`

| File | Line(s) | Classification |
|---|---|---|
| `.djinn/decisions/adr-038-*.md` | 241 | **Superseded/removal documentation** |
| `.djinn/requirements/split-oversized-production-hubs-agent-mcp-bridge-roadmap.md` | 44 | **Historical requirements context** — lists `prompt_eval` as a module name in the coordinator split pattern. No source file `prompt_eval.rs` exists; the module was removed by 3x0w. |
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** |

#### `agent_amend_prompt`

| File | Line(s) | Classification |
|---|---|---|
| `.djinn/decisions/adr-038-*.md` | 16, 241 | **Superseded/removal documentation** |
| `.djinn/decisions/adr-051-*.md` (both variants) | 19–24, 49, 50, 63, 64 | **Superseded/removal documentation** — 1hqf added `> z5f9 note:` annotations and strikethrough |
| `server/crates/djinn-agent/src/prompts/tests.rs` | 256, 269, 270, 275, 288, 289, 593, 596, 599, 600, 604, 609, 612, 613, 617 | **Negative regression guards** — all assertions are `!contains` or `!exposes` (asserting the tool is NOT present) |
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** |

#### `append_learned_prompt`

| File | Line(s) | Classification |
|---|---|---|
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** — zero runtime/code hits |

#### `resolve_pending_amendment`

| File | Line(s) | Classification |
|---|---|---|
| `server/docs/learned-prompt-cutover.md` | (this file) | **Explicit cutover/removal documentation** — zero runtime/code hits |

### 2. Migration and SQLx/offline metadata consistency

#### Drop migration

**File:** `server/crates/djinn-db/migrations_postgres/101_drop_learned_prompt_schema.sql`
**Content:**
```sql
-- Drop learned_prompt_history table and agents.learned_prompt column.
-- Final cutover for proposal z5f9: prerequisite epics t8p8 (harvest),
-- 3x0w (runtime removal), and 3sle (generated/UI cleanup) are closed.
--
-- Order: drop dependent history table first, then the column on agents.
DROP TABLE IF EXISTS learned_prompt_history;

ALTER TABLE agents
    DROP COLUMN IF EXISTS learned_prompt;
```

**Initial schema (1_initial_schema.sql) — NOT modified:** The initial migration at lines 457, 473–485 correctly retains the `learned_prompt` column and `learned_prompt_history` table. Per the Server Migrations Guard, applied migration files are immutable — sqlx checksums them on boot and refuses to start if they changed. The new migration `101` is the correct way to remove these objects for existing databases.

#### SQLx offline metadata

```
$ git grep -n 'learned_prompt' -- 'server/.sqlx/'
(no output)
```

**Result:** Zero hits in `server/.sqlx/query-*.json`. The SQLx offline metadata contains no learned-prompt references. No metadata updates were needed — the 3sle generated-artifact epic (kutw, xij7) already regenerated the metadata after the runtime query removals, and the drop migration does not introduce new queries.

#### Fresh-DB consistency

The migration ordering is:
1. `1_initial_schema.sql` — creates `learned_prompt` column and `learned_prompt_history` table
2. `101_drop_learned_prompt_schema.sql` — drops both objects

On a fresh database, sqlx applies all migrations in sequence. The column and table are created in migration 1 and immediately dropped in migration 101. This is correct sqlx migration behavior — the initial schema is the canonical creation point for historical ordering, and the drop migration handles the removal.

### 3. No-touch invariant checks

#### `server/crates/djinn-coordinator/src/refinement*.rs`

```
$ git diff HEAD -- server/crates/djinn-coordinator/src/refinement*.rs
(no output)
```

**Result:** Zero modifications to any refinement*.rs file. The coordinator refinement implementation was not touched by the final cutover work.

Files confirmed present and unmodified:
- `server/crates/djinn-coordinator/src/refinement.rs`
- `server/crates/djinn-coordinator/src/refinement_recovery.rs`
- `server/crates/djinn-coordinator/src/refinement_dispatch.rs`
- `server/crates/djinn-coordinator/src/refinement_outcome.rs`
- `server/crates/djinn-coordinator/src/refinement_e2e_evidence_regression_tests.rs`
- `server/crates/djinn-coordinator/src/refinement_recovery_tests.rs`
- `server/crates/djinn-coordinator/src/refinement_pool_watchdog_tests.rs`
- `server/crates/djinn-coordinator/src/refinement_dor_status_tests.rs`
- `server/crates/djinn-coordinator/src/refinement_cap_tests.rs`
- `server/crates/djinn-coordinator/src/refinement_evidence_resume_tests.rs`

#### Memory/belief implementation paths

No memory or belief implementation files were modified by this task.

#### Code-graph implementation paths

No code-graph implementation files (`server/crates/djinn-graph/src/**/*.rs`) were modified by this task.

### 4. Classification summary

| Category | Count | Status |
|---|---|---|
| **Negative regression guards** (test code asserting removed symbols are NOT present) | ~30 lines across 2 files | ✅ Preserved — prevent re-introduction |
| **Harvest artifacts/tooling** (t8p8: runbook, scripts, fixtures, contract test) | ~100 lines across 6 files | ✅ Preserved — pre-removal audit evidence |
| **Superseded/removal documentation** (ADR-038, ADR-051 with z5f9 annotations) | ~20 lines across 3 files | ✅ Annotated by 1hqf with supersession blocks |
| **Initial schema migration** (1_initial_schema.sql — immutable applied migration) | 5 lines in 1 file | ✅ Retained per CI guard; dropped by migration 101 |
| **Drop migration** (101_drop_learned_prompt_schema.sql) | 9 lines in 1 file | ✅ New from 5wyg |
| **Historical requirements context** (coordinator module split roadmap) | 1 line in 1 file | ✅ Lists `prompt_eval` as a module name — no source file exists |
| **Cutover documentation** (this file) | 1 file | ✅ Evidence record |
| **Runtime/API/UI code paths** | 0 hits | ✅ Fully removed by 3x0w and 3sle |
| **SQLx offline metadata** | 0 learned-prompt references | ✅ Consistent — no updates needed |

### 5. This task's changes

The only file change introduced by this task is appending this final evidence section to `server/docs/learned-prompt-cutover.md`. No runtime code, migration, UI, generated artifact, or harvest tooling was modified. The drop migration `101_drop_learned_prompt_schema.sql` is owned by task 5wyg (PR #1814) and is referenced in this evidence but not included in this task's commit.
