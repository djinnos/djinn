---
title: Remove verified dead code across agent + repo_map + repo_graph — Roadmap
type: requirement
tags: ["planning","dead-code","djinn-agent","repo-map","repo-graph"]
---

# Remove verified dead code across agent + repo_map + repo_graph — Roadmap

## Goal
Remove the confirmed dead-code pockets listed in epic `pfq7` without touching the explicit out-of-scope items.

## Wave 1 plan
Create focused removal tasks grouped by file/seam so workers can delete code, fix any resulting compile fallout, and prove `cargo check --tests` stays clean.

Planned task groups:
1. Remove unused `djinn-agent` task-merge file/seam (`crates/djinn-agent/src/task_merge.rs`) and any module wiring that becomes dead.
2. Remove unused verification-gate functions from `crates/djinn-agent/src/actors/slot/verification.rs`.
3. Remove unused coordinator dead code (`DEFAULT_PLANNER_PATROL_MINUTES` in `rules.rs`, `with_consolidation_runner` in `types.rs`) while preserving in-scope coordinator behavior.
4. Remove unused repo-map single-flight helper from `src/repo_map/indexing.rs`.
5. Remove unused `RepoGraph` accessor methods marked dead in `src/repo_graph.rs`.

## Scope guardrails
Keep these explicitly out of scope:
- `coordinator/consolidation.rs::run_for_group`
- `github_api/checks.rs::_keep_public_type`
- `oauth/github_app.rs` serde fields `refresh_token` / `expires_at` / `refresh_token_expires_at`

## Done criteria
- Each confirmed-dead item in the epic description is removed.
- No new `#[allow(dead_code)]` is added.
- `cargo check --tests` passes.
- No behavior change beyond dead-code deletion.

## Notes
If repo inspection shows some items share a small compile-fix seam, blockers may be added to keep overlapping edits serialized.
