---
title: Implement ADR-052: Pulse proposal inbox, ask-architect trigger, proposed epic lane roadmap
type: design
tags: ["adr-052","pulse","proposal-inbox","roadmap"]
---

# Implement ADR-052: Pulse proposal inbox, ask-architect trigger, proposed epic lane — Roadmap

## Status
Wave 1 implementation landed on main: Pulse now exposes the ask-architect entrypoint, proposal inbox/detail surface, accept/reject review actions, and sidebar badge/toast notification path. The plumbing dependency epic [[design/fix-proposal-pipeline-plumbing-defects-adr-052-defects-uncovered-roadmap]] is closed.

## Planner decision: proposed epic lane
For v1, use **Option C (hybrid projection)** from ADR-052.

- Filesystem drafts under `.djinn/decisions/proposed/` remain the review-gated source of truth before acceptance.
- Pulse renders the inbox from `propose_adr_*` results, not from `epics.status = proposed` rows.
- DB `epics.status = proposed` support remains a projection seam for accepted/provisioned shells and future evolution, not the canonical inbox source.

Rationale:
1. Preserves ADR-051's review gate around proposal drafts.
2. Matches the shipped proposal tool surface and current Pulse list/detail implementation.
3. Avoids silently overriding the architect's filesystem-first model while still leaving the DB seam available for accepted/projection workflows.

## Wave 1 completed
- `dnd9` — Ask architect modal and spike creation flow.
- `timv` — `ArchitectProposalsSection` list, filters, and detail panel.
- `xqp1` — Accept/reject review actions in Pulse detail.
- `vhzj` — Pulse nav badge and same-session draft toast.

## Gaps found after wave 1 review
1. **Typed MCP client parity gap**: generated desktop MCP types expose `propose_adr_list` and `propose_adr_show`, but not `propose_adr_accept` / `propose_adr_reject`, so the UI currently needs a local untyped action helper around review actions.
2. **Proposal age/polish gap**: desktop proposal parsing currently hard-codes `modifiedAt: null`, so the ADR-052 age display is only partially realized even though the UI has a slot for it.
3. **End-to-end dogfood gap**: the epic acceptance criteria still need an explicit UI-driven regression that covers ask-architect → proposal appears → accept/reject behavior → epic shell visibility / breakdown handoff.

## Next wave
Create a final verification/polish wave focused on contract parity, age metadata, and end-to-end Pulse dogfood coverage.

### Task candidates
1. Regenerate and adopt typed desktop MCP contracts for proposal review actions.
2. Add `mtime`/age metadata to proposal list responses and surface real timestamps in Pulse.
3. Add a dogfood-style integration test for the full Pulse proposal workflow, including accepted epic-shell handoff.

## Exit criteria for epic closure
Close the epic once:
- the typed-client parity gap is removed or explicitly documented as intentionally deferred,
- proposal age metadata is surfaced or explicitly deferred from v1 scope,
- and a regression proves the Pulse-only dogfood flow described in ADR-052 acceptance criteria.

## Relations
- [[decisions/adr-052-pulse-proposal-inbox-ask-architect-trigger-and-virtual-proposed-epic-lane]]
- [[decisions/adr-051-planner-as-patrol-and-architect-as-consultant]]
- [[design/fix-proposal-pipeline-plumbing-defects-adr-052-defects-uncovered-roadmap]]
