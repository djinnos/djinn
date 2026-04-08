---
title: "ADR-052: Pulse Proposal Inbox, Ask-Architect Trigger, and Proposed Epic Lane"
type: adr
tags: ["adr","pulse","architect","proposal-pipeline","adr-051","epic-c"]
---

# ADR-052: Pulse Proposal Inbox, Ask-Architect Trigger, and Proposed Epic Lane

## Status: Proposed — Reconstructed from lost architect session

Date: 2026-04-08

Originating spike: `ih6u` — *"Finish ADR-051 Epic C: Pulse surfaces + proposed epic lane"*

Extends: [[ADR-051: Planner-as-Patrol, Architect-as-Consultant, ADR-to-Epic Pipeline, and Auto-Dispatch Reentrance Guards]]

Related: [[ADR-050: Architect/Chat Code-Graph Consolidation]], [[ADR-046: Chat-Driven Planning]]

## ⚠️ Reconstruction notice

**This ADR is a partial reconstruction.** The original draft was produced by an
architect session on spike `ih6u` (2026-04-08, ~45 min run, ~14,263 characters)
and then lost when the task-close flow classified the spike as a
"simple-lifecycle task — no PR needed" and tore down the worktree + worktree-scoped
memory DB without committing the content. See the *Defects uncovered* section
below — this ADR exists in parallel with a companion epic that tracks the
plumbing fixes required to prevent the same loss from happening again.

The decisions, Pulse IA, and planner breakdown below were reconstructed from the
architect's tool-call trace captured at
`~/.claude/projects/-home-fernando-git-djinnos-djinn/.../mcp-plugin_djinn_djinn-task_timeline-1775684072382.txt`
before that file was garbage-collected. Verbatim quotes from the original are
marked with `> [original]`. Everything else is faithful summary. The alternatives
considered and the long-form consequences section from the original are lost;
they should be re-derived during implementation review if needed.

---

## Context

ADR-051 positioned the architect as a consultant dispatched via `spike` tasks
that write ADR drafts to `.djinn/decisions/proposed/`, and introduced the
`propose_adr_list` / `propose_adr_show` / `propose_adr_accept` /
`propose_adr_reject` MCP tools so humans can review and promote those drafts.
Epics A (tool-surface migration) and B (reentrance guards) shipped on
2026-04-08. **Epic C — the Pulse surfaces that make the pipeline usable — did
not.** There is no way for a user to ask the architect a question, no way to
see pending proposals without manually tailing the filesystem, and no way to
accept a proposal from the UI. The pipeline exists only as an MCP contract.

This ADR answers the seven design questions in spike `ih6u` and gives the
Planner enough structure to slice Epic C into parallelizable work.

## Decision

### 1. Data model for the "proposed epic lane"

> [original] **Do not create `epics` rows for unaccepted proposals.**
> Epic-shaped architect output remains a single source of truth on disk under
> `.djinn/decisions/proposed/`, represented as an ADR draft with
> `work_shape: epic`. The Pulse "proposed epic lane" is therefore a **virtual
> lane**, derived from `propose_adr_list(project)` filtered to
> `work_shape == "epic"`, not a new `epics.status = proposed` state.

Rationale from the original: avoid duplicate truth between DB rows and draft
files, preserve the ADR-051 review gate as the one promotion path, avoid
coordinator hazards (auto-dispatch firing on a proposed epic before acceptance),
and remain aligned with the already-shipped `propose_adr_accept` behavior.

**⚠️ Contradiction with current code — must be resolved during implementation.**
At the time of this reconstruction, `epic_create` already accepts
`status: "proposed"`, `auto_breakdown: false`, and `originating_adr_id`, with
doc comments explicitly flagging them as ADR-051 Epic C scaffolding. A partial
DB-backed implementation of the lane therefore *already exists in the
codebase* — the architect's spike context did not surface this. The real
choice is now:

- **A.** Honor the architect's decision: remove the `proposed` status from
  `epic_create`, keep the lane pure-filesystem, treat the existing DB fields
  as stillborn scaffolding.
- **B.** Override the architect's decision: keep the `epics.status = proposed`
  DB representation as the canonical lane, and have `propose_adr_list` + the
  Pulse inbox surface both the filesystem drafts and the proposed DB rows as
  a unified view.
- **C.** Hybrid: filesystem drafts remain the write target (ADR-051 review
  gate intact), but a DB row is created on draft write and the status flips
  to `open` on `propose_adr_accept`. The DB row is a projection, not the
  source of truth.

The implementation epic must explicitly pick one of A/B/C before any Pulse
work starts. Recommendation: **C** — minimal churn, resolves the half-built
DB support without discarding it, and keeps the single write path.

### 2. Pulse information architecture

> [original] Sidebar/nav: keep existing `Pulse` entry, add an unread badge to
> it. Pulse page layout: freshness strip / existing structural panels / new
> `Architect Proposals` section near the top. Segmented tabs: `All`,
> `Epic-shaped`, `Architectural`, `Task/Spike`. List pane on the left,
> detail/review pane on the right or slide-over on narrower widths.

Component hierarchy:

```
App Sidebar
└── Pulse (nav entry, unread badge)

PulsePage
├── FreshnessStrip
│   ├── graph freshness indicator
│   ├── architect-active indicator
│   └── AskArchitectButton         (NEW)
├── ArchitectProposalsSection      (NEW, near top)
│   ├── ProposalsToolbar           (segmented tabs)
│   ├── ProposalList
│   │   └── ProposalCard[]         (title, work_shape badge,
│   │                               originating_spike_id, age)
│   └── ProposalDetailPanel        (slide-over on narrow widths)
│       ├── frontmatter summary
│       ├── markdown body
│       └── actions: Accept | Reject | Defer
├── HotspotsPanel                  (existing)
├── DeadCodePanel                  (existing)
├── CyclesPanel                    (existing)
└── BlastRadiusPanel               (existing)
```

The proposal inbox lives *inside* Pulse, not as a new top-level nav
destination, and not as a new Kanban column. Pulse already means "things that
need your attention" — pending proposals belong in that mental model.

### 3. "Ask architect" UX

Minimum viable modal:

- **Question** — required, free-text `<textarea>`, becomes the spike task
  title (trimmed) and the first line of the description.
- **Context** — optional, free-text, appended to the spike description under
  a `## Context` heading.
- **Project** — implicit, taken from current Pulse context.
- **Submit** calls `task_create(issue_type="spike", title=..., description=...)`
  and navigates optimistically to the new spike's task detail view, where the
  user can watch it dispatch and complete.

Rejected alternatives from the original session: structured fields for urgency,
explicit scope selection, dry-run preview. All deferred to v2 to keep the
trigger frictionless.

### 4. Notification cadence

> [original] Badge-first via `propose_adr_list(project).items.length`, polled
> by React Query; personal-attribution toast only if user is in the project
> and spike was Pulse-originated; no SSE in v1.

- Sidebar badge on the `Pulse` nav entry: count of drafts in
  `propose_adr_list`.
- React Query refetches on interval (e.g. 30s) and on window focus.
- When a user-originated spike (dispatched via "Ask architect") produces a
  draft, a toast notifies that specific user. Attribution is done via a
  session-local map of `spike_id → originator`, not a DB field — lost on
  reload, which is acceptable.
- **No SSE / push / websocket in v1.** Server-sent events are a v2 polish.

### 5. Review actions

Four actions exposed on `ProposalDetailPanel`:

- **Accept** — opens a sub-form with: `title override` (defaults to draft
  title), `create_epic` checkbox (defaults true, disabled + hidden for
  `work_shape: architectural`), `auto_breakdown` checkbox (defaults true).
  Calls `propose_adr_accept(id, title, create_epic, auto_breakdown)`.
- **Reject** — requires a non-empty `reason`. Calls `propose_adr_reject(id)`
  and threads the reason via `task_comment_add` on `originating_spike_id`.
- **Defer** — no-op client-side marker; hides the proposal from the default
  inbox view for N days. No backend state. (v1 can ship without this if it's
  costly.)
- **Edit-before-accept** — limited to the accept sub-form fields (title,
  `auto_breakdown`, `create_epic`). Full markdown editing of the draft body is
  deferred to v2 and would require a new `propose_adr_update` MCP tool.

### 6. Rejection feedback loop

When a proposal is rejected:

1. Required `reason` field captured in the UI.
2. Passed to `propose_adr_reject(id, reason)` — the MCP tool must be extended
   to accept a reason (currently it likely does not).
3. Server threads the reason back as a `task_comment_add` on the draft's
   `originating_spike_id`, formatted as:
   ```
   Proposal rejected by <reviewer> at <timestamp>.
   Reason: <reason>
   ```
4. Fallback if `originating_spike_id` is absent or the spike task has been
   GC'd: write a memory audit note in `research/architect-rejections/` so the
   feedback is at least preserved.

This gives the architect (via `task_timeline` on the originating spike) a way
to see *why* its output was rejected, which is the minimum signal needed for
future prompt tuning or few-shot learning.

### 7. Architectural drafts (`work_shape: architectural`)

These drafts never produce epics — they are pure decisions, ideally short.
They share the inbox with an `Architectural` tab/badge, and on acceptance
leave the inbox entirely and appear only in the decisions browser (wherever
that surfaces today — likely file-system-driven). No board-side effect, no
Planner dispatch.

## Defects uncovered by spike `ih6u`

The spike itself exposed seven defects in the proposal pipeline. **All seven
must be addressed before Epic C's implementation work can land safely** —
they block the ability to even re-run the spike and get a surviving draft.

| # | Defect | Symptom |
|---|---|---|
| 1 | Task-close flow classifies content-producing spikes as "simple-lifecycle task — no PR needed" | Architect's `SubmitForMerge` short-circuited; worktree torn down with draft inside; no commit, no branch, no PR |
| 2 | `memory_write` ignores `work_shape: epic` / `Status: Proposed` frontmatter when choosing storage folder | Draft with proposal frontmatter landed in `.djinn/decisions/`, not `.djinn/decisions/proposed/` |
| 3 | `memory_write` return value reports a main-repo path while the file materializes inside the worktree | Misleads agent's subsequent recovery attempts |
| 4 | Worktree sandbox denies `mkdir`/`cp` into `.djinn/decisions/proposed/` | Agent cannot self-relocate mis-routed drafts |
| 5 | Worktree-scoped memory DB is not merged back to the main project memory on task close | The sole surviving artifact (memory note id `019d6ef4-...`) evaporated with the worktree |
| 6 | `propose_adr_list` / `propose_adr_show` / `propose_adr_accept` / `propose_adr_reject` MCP tools are schema-advertised but not dispatched by the upstream daemon | Every call returns `"unknown MCP tool"` |
| 7 | Agent can call `task_comment_add` before verifying a follow-up action succeeded | Phantom comment on `ih6u` claims the draft was placed in `proposed/` even though every `cp` attempt failed |

Defect 1 is the root cause of 2–5; the others can be fixed independently.
Defect 6 is independent and blocks the Pulse review surface entirely.

## Planner breakdown hints

> [original] 1. Pulse ask-architect trigger (modal, `task_create(issue_type="spike")`, optimistic navigation)
> 2. Proposal inbox surface (list from `propose_adr_list`, detail from `propose_adr_show`, badges/filtering/empty states)
> 3. Review actions (accept, reject-with-reason, comment-threading to originating spike)
> 4. Notification/badge (sidebar count, RQ refresh, optional per-origin toast)
> 5. API polish follow-ups (mtime in list response, proposal-edit tool, proposal SSE events)
>
> Coupling note: slices 1/2/4 independent; slice 3 depends on rejection
> persistence decision but can start with spike comments; slice 5 optional.

Reconstruction note: slices 2 and 3 are **blocked** on plumbing defect #6
(MCP dispatch). Slice 1 is blocked on defect #1 if it is to produce a
reviewable draft. Recommend the Planner opens the plumbing epic as a hard
dependency of every Pulse slice.

## Consequences

(The original draft's consequences section is lost. Minimum placeholder.)

**Positive:**
- Pulse becomes the single pane for all architect interaction
- Proposal pipeline becomes usable without MCP CLI knowledge
- Dogfood feedback loop for ADR-051 closes

**Negative:**
- Seven plumbing defects must land before the visible work ships
- Data-model contradiction with existing `epic_create` scaffolding forces an
  explicit A/B/C choice before implementation
- v1 skips SSE, full markdown editing, and structured ask-architect fields

## References

- Originating spike: `ih6u`
- ADR-051: `.djinn/decisions/adr-051-planner-as-patrol-and-architect-as-consultant.md`
- Proposal backend: `server/crates/djinn-mcp/src/tools/proposal_tools.rs`
- Architect role: `server/crates/djinn-agent/src/roles/architect.rs`
- Dispatch routing: `server/crates/djinn-agent/src/roles/mod.rs:164`
- `epic_create` proposed-epic scaffolding: `server/crates/djinn-mcp/src/tools/epic_tools.rs` (search for `status: "proposed"`)
