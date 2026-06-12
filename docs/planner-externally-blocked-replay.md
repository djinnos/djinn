# Planner externally-blocked replay runbook

This staged replay documents the 4lzx-style externally-blocked prune-and-close scenario. It is the lightweight companion to a live agent-loop regression: reviewers can inspect and re-run the focused prompt-policy replay without Docker, Postgres, Kubernetes, operator credentials, or a deployed Djinn control plane.

## Scenario shape

The synthetic fixture models an epic and an open planning task where all implementable work is already done. The only remaining acceptance criteria ask for unavailable external or operator-only proof, for example:

- running a Docker/Postgres integration stack and proving migrations against a live database;
- deploying or inspecting a Kubernetes operator rollout from cluster state;
- authenticating to a live Djinn deployment and capturing operator-only evidence.

Those proof items are operational context for an operator checklist, not worker acceptance criteria. A task-run worker is not expected to have Docker daemon access, a live Postgres test environment, cluster/operator RBAC, or production Djinn authentication. The convergence behavior under review is therefore the Planner's ability to repair/prune invalid criteria and close the planning loop, not to manufacture external proof from inside the worker pod.

## Expected planner convergence

For the 4lzx-style replay, one Planner session should converge with this action sequence:

1. Repair or prune the invalid external-proof acceptance criteria with `task_update`, or record equivalent roadmap/documentation rationale when the criterion belongs outside worker-verifiable AC.
2. Add a comment/rationale explaining that Docker/Postgres/Kubernetes/operator-auth proof is unavailable to task-run agents and has been moved out of acceptance criteria.
3. Close the completed epic with `epic_close`.
4. Finish the planning task with `submit_grooming(decision="close")` so the planning task is not dispatched again.

## Forbidden outcomes

The replay is a regression guard for planner loops. A passing review must confirm the Planner does **not**:

- call `submit_grooming(decision="escalate")` for missing external infrastructure or credentials;
- create or retry worker tasks whose only purpose is to gather Docker/Postgres/Kubernetes/operator-auth proof;
- leave the planning task open or otherwise eligible for repeated dispatch after the epic has no implementable work left.

## Focused replay command

Run the focused Rust test from the repository root:

```bash
cd server && cargo test -p djinn-agent planner_4lzx_externally_blocked_replay_prunes_and_closes_in_one_session --all-features
```

The test lives in `server/crates/djinn-agent/src/prompts.rs`. It builds a synthetic `4lzx-epic` / `4lzx-planning` fixture with only externally-blocked criteria remaining, renders the Planner prompt policy, and asserts that the modeled replay produces the prune/rationale/`epic_close`/`submit_grooming(decision="close")` path while avoiding escalation, worker retries for external proof, and redispatch loops.

No external infrastructure credentials are required for this command. It exercises prompt-policy convergence only; Docker, Postgres, Kubernetes, operator RBAC, and live Djinn authentication are intentionally outside worker acceptance for this replay.

## Reviewer checklist

- [ ] Confirm this document names the 4lzx-style externally-blocked prune-and-close scenario.
- [ ] Run the focused command above, or inspect its current CI result, without provisioning Docker/Postgres/Kubernetes/operator credentials.
- [ ] Confirm the replay fixture describes an epic/planning task with no implementable work left and only unavailable external proof criteria remaining.
- [ ] Confirm the expected actions include criteria repair/prune via `task_update` or equivalent rationale, comment/rationale, `epic_close`, and `submit_grooming(decision="close")`.
- [ ] Confirm the forbidden actions remain forbidden: escalation, retry worker tasks for external proof, and any path that leaves the planning task eligible for redispatch.
- [ ] Treat any desired live Docker/Postgres/Kubernetes/operator-auth proof as an operator runbook/evidence concern, not as acceptance criteria for worker tasks in this replay.
