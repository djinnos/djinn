---
title: Narrow current-note memory hygiene follow-up
type: reference
tags: ["memory","triage","hygiene","planner","broken-links","orphans"]
---

# Narrow current-note memory hygiene follow-up (2026-04-14 refresh)

Context: `memory_health()` currently reports **124 broken links** and **1042 orphans**. Current patrol evidence still shows that these elevated counts are **not** primarily fresh canonical-note regressions. They are dominated by two long-classified backlog shapes:

1. **Tolerated historical broken-link debt** in older `decisions/*` and `reference/*` notes
   - repeated legacy ADR title aliases
   - shorthand ADR numbers/forms
   - generic singleton shorthand such as `[[Roadmap]]`
   - occasional placeholder/prose targets like `[[wikilinks]]`, `[[target]]`, or old draft labels
2. **Orphan-heavy inventory folders** whose volume should be monitored separately from actionable canonical-note defects
   - `cases/*`
   - `reference/repo-maps/*`

## Refreshed classification boundary

### Broken-link backlog: mostly tolerated historical alias debt
Current `memory_broken_links()` sampling still concentrates on:
- full ADR-title aliases in historical ADR/reference notes
- short ADR forms like `ADR-006`, `ADR-009`, `ADR-014`, `ADR-022`
- generic `Roadmap` shorthand in older notes
- a small minority of placeholder/prose wikilinks

Interpretation for future patrols:
- Treat this as **real note-content debt**, but mostly **historical and tolerated** rather than an active tooling defect.
- Do **not** reopen broad historical alias normalization just because gross broken-link counts stay high.
- Only escalate when broken links appear in a **current canonical note** whose navigation quality matters right now.

### Orphan backlog: mostly inventory, not emergency cleanup
Current `memory_orphans()` output remains dominated by:
- large `cases/*` inventory
- large `reference/repo-maps/*` inventory

Interpretation for future patrols:
- `cases/*` remains a **retrieval-oriented historical inventory bucket**. Count growth alone is not a cleanup trigger.
- `reference/repo-maps/*` remains **intentional artifact inventory**. Count growth alone is not a cleanup trigger.
- Patrol should only escalate orphan findings when they cluster in **current canonical docs** such as active roadmap/requirement/reference/design notes whose value depends on navigational linkage.

## Current actionable slice

The previously identified narrow current-note cleanup slice has already been handled by [[nwg2]]. That work normalized the small set of named current canonical notes that were worth fixing immediately without reopening historical cleanup.

As of this refresh, the actionable slice is therefore:
- **no broad standing cleanup batch**
- only **opportunistic narrow cleanup** if future patrols find newly broken links or orphan defects in active canonical notes
- continue to leave historical `Roadmap` shorthand and ADR-title alias debt untouched unless a note is being edited for another reason

## Patrol operating rule

When counts are elevated:
1. Check whether broken links are still dominated by historical ADR-title aliases / `Roadmap` shorthand.
2. Check whether orphans are still dominated by `cases/*` and `reference/repo-maps/*`.
3. If yes, classify the backlog as **known inventory + tolerated historical debt**, not as a new hygiene incident.
4. Only open follow-up work for a **tiny current-note subset** with clear canonical-value defects.
5. Do **not** open mass relinking, alias-support, or broad historical editorial cleanup from these counts alone.

## Relationship to the broader backlog triage

This note is the narrow patrol-facing companion to [[reference/project-memory-broken-link-and-orphan-backlog-triage]].

Use the broader triage note for:
- rationale on why raw counts remain high
- background on tolerated orphan-heavy folders
- classification of historical alias debt

Use this note for:
- the current patrol rule of thumb
- the boundary that the earlier narrow cleanup is already complete
- deciding whether a new finding is genuinely actionable or just more of the same classified backlog
