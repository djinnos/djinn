---
title: Narrow current-note memory hygiene follow-up
type: reference
tags: ["memory","triage","planner","adr-057"]
---

# Narrow current-note memory hygiene follow-up

This note records the intentionally narrow follow-up slice after project memory backlog triage.

## Purpose

When memory-health counts stay elevated due mostly to known historical alias debt and orphan-heavy inventory, document a small explicitly owned cleanup slice for current canonical notes instead of reopening the full backlog.

## Scope boundary

Treat this as **narrow active-memory maintenance**, not a broad cleanup campaign.

In scope:
- verify and preserve the canonical follow-up surface for the current-note cleanup decision
- maintain stable memory refs used by active planning/epic context
- keep the documented cleanup slice limited to a tiny set of current canonical notes

Out of scope:
- mass historical ADR-title alias normalization
- broad `[[Roadmap]]` shorthand cleanup across archival notes
- mass relinking of `cases/*`
- mass relinking or suppression of `reference/repo-maps/*`
- reopening broken-link/orphan backlog work beyond the explicitly named current canonical-note slice

## Follow-up slice

The earlier actionable slice was intentionally tiny:
- `decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface`
- `decisions/adr-053-semantic-memory-search-candle-embeddings-with-sqlite-vec`
- `reference/repository-understanding-and-memory-freshness-upgrade-path`
- `reference/project-memory-broken-link-and-orphan-backlog-triage`

That slice was chosen because these are current canonical notes whose broken title-style links affected active planning and patrol interpretation more than the broader historical backlog.

## Patrol interpretation

Current elevated counts remain dominated by two tolerated buckets unless they surface an active canonical-note defect:
- historical ADR-title / `Roadmap` alias debt in older notes
- orphan-heavy inventory in `cases/*` and `reference/repo-maps/*`

Future patrols should escalate only when defects are concentrated in active canonical notes or when an active epic/task depends on a memory ref that does not resolve canonically.

## Relations
- [[reference/project-memory-broken-link-and-orphan-backlog-triage]]
- [[decisions/adr-057-proposal-fuse-mounted-memory-filesystem-as-the-primary-agent-interface]]
- [[cases/narrow-current-note-memory-hygiene-follow-up-amid-tolerated-backlog-debt]]
