# Agent Readiness Candidate Criteria

Candidate guardrail criteria identified during proposal/epic work that are
forwarded here for inclusion in the Agent Readiness readiness-checker catalog
(proposal `m2z3`).

Each candidate has an ID, sourcing provenance, evidence pattern, and
anti-patterns so that reconcilers can score the guardrail consistently and
without ambiguity.

---

## CONTRACT-API-001

**Status:** Candidate — pending inclusion in Agent Readiness Phase 0+ catalog.

**Suggested level:** L2 or L3 (Build / Code Quality pillar — contract safety).

**Suggested remediation key:** `enforce-api-contract-drift-check`.

**Wording:**

> UI-consumed API response types are generated from a single source or
> schema-checked in CI, and critical fields have render-smoke coverage.

### Source provenance

- **Proposal:** `zhwn` — _Fix Usage & Analytics cost-basis split: empty KPI
  totals + subscription/coding-plan sessions miscounted as Actual API spend_
  (`019f06ba-9327-7de1-a52b-ee5e3c778067`).
- **Epic:** `4iwt` — _Add Usage Overview render smoke coverage and forward
  REST-admin contract learning_.
- **Roadmap:** `design/4iwt-roadmap`.
- **Memory note:**
  `reference/agent-readiness-phase-0-specification-addendum-round-2-advocate`
  contains the identical candidate criterion for durable cross-reference.
- **Target proposal:** `m2z3` — _Agent Readiness_ (`019ee04e-7e0c-7961-8ee9-18170d605d8f`).

### Evidence pattern (what counts as "covered")

All of the following must hold:

1. **Single source of truth.** The server DTO / JSON Schema is the canonical
   source for the UI-consumed response type, OR an equivalent shared-schema
   test validates every frontend-consumed field in CI.
2. **No hand-mirrored drift.** The UI API module imports or re-exports the
   generated / schema-checked type instead of maintaining a drift-prone hand
   mirror for critical response fields.
3. **CI runs the check.** The generation or drift-check step runs in CI and
   fails the build on mismatch.
4. **Render smoke for critical fields.** At least one render smoke test
   proves critical generated fields appear as user-visible values rather
   than fallback placeholders (e.g. `EM_DASH`, `0`, `N/A`).

### Anti-patterns (what counts as "not covered")

- Hand-authored optional TypeScript fields that can disappear from backend
  JSON without a CI failure.
- Endpoint tests that only assert a generic `label` / `value` shape while
  omitting UI-rendered critical fields.
- Contract checks without a render smoke for fields whose absence would be
  hidden by UI fallback values.

### Preferred implementation order (for future reconcilers)

1. Check whether worker tools expose a proposal feedback / comment mechanism
   for `m2z3`. If available, add this candidate criterion there and mention
   source epic `4iwt` / proposal `zhwn`.
2. If no proposal feedback tool is available, reference this file
   (`docs/agent-readiness-candidate-criteria.md`) from any new
   readiness-criteria catalog entries.
3. Do not directly rewrite proposal `m2z3` acceptance criteria — this is
   forwarding candidate learning, not unilateral spec replacement.
4. When the readiness checker crate (`djinn-readiness`) is implemented, add
   `CONTRACT-API-001` to `criteria.rs` with the evidence pattern above as the
   pass/fail rule.

---

## CI-TIMEOUT-001

**Status:** Candidate — pending inclusion in the Agent Readiness Phase 0+
catalog.

**Suggested level:** L2 (Build pillar — CI execution bounds and runtime
evidence).

**Suggested remediation key:** `enforce-ci-timeout-contract`.

**Wording:**

> Required CI composition has finite, offline-verified timeout coverage; when
> sufficient unambiguous GitHub Actions evidence is available, its configured
> bounds receive an advisory p95-based timeout recommendation.

### Source provenance

- **Proposal:** `g8ho` — _CI hard timeouts: measure with Agent Readiness and
  enforce in native CI configuration_.
- **Epic:** `wb2t` — _Add CI-TIMEOUT-001 runtime evidence and recommendations
  to Agent Readiness_.
- **Roadmap:**
  `design/wb2t-ci-timeout-001-agent-readiness-roadmap`.
- **Offline-contract dependency:** closed epic `8ijd`, specifically its
  `.github/ci-timeouts.json` manifest and `scripts/check-ci-timeouts.mjs`
  checker. This candidate reuses that contract as-is; it does not recreate or
  modify the manifest or checker.
- **Target proposal:** `m2z3` — _Agent Readiness_.

### Composition selector and authoritative context inventory

The composition selector is the checked-in v1
`.github/ci-timeouts.json` manifest: its `terminalRoots` select the terminal
CI compositions, and its `covered` identities declare the complete resolved
composition. An identity is a canonical workflow path plus literal job ID
(`.github/workflows/<workflow>.yml#<job>`), with `=>` call-chain prefixes for
reusable-workflow members. The existing offline checker expands `needs` and
local reusable workflows from each selected terminal root and verifies the
sorted manifest inventory, finite `timeout-minutes`, and transitive coverage.

The assessment must also obtain the authoritative required-context inventory
for the protected target branch from GitHub branch-protection required-status
checks and applicable repository rulesets, then compare that inventory with
the manifest's terminal-context declarations. That remote comparison is not a
substitute for the offline checker: unavailable branch-protection or ruleset
metadata, or metadata that differs from the declarations, reports
`required-context-inventory-unverified`. It must not make ordinary repository
CI network-dependent or overturn the offline checker's decisive stable
repository-coverage result.

### Results and evidence contract

Report two separately named results:

1. **Coverage compliance** is the stable, fail-closed result of the existing
   offline manifest/checker. A finite static bound remains compliant or
   noncompliant based on that local contract, independent of runtime evidence
   age, GitHub availability, or expiring source URLs.
2. **Recommendation confidence** is advisory runtime-evidence quality. It
   reports sufficient evidence and a recommendation, or explicit evidence
   gaps; it never changes coverage compliance merely because evidence is old,
   missing, or unavailable.

The runtime collector uses these GitHub REST endpoint families (with the
resolved repository, workflow, and run values):

- workflow metadata: `GET /repos/{owner}/{repo}/actions/workflows`;
- workflow runs: `GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs`;
- attempt-1 jobs: `GET /repos/{owner}/{repo}/actions/runs/{run_id}/attempts/1/jobs`.

For runtime evidence, the provider-exposed identity key is **exactly**:

`workflow path + exact rendered job name + normalized/sorted runner labels + runner group`.

The rendered job name deliberately keeps distinct rendered matrix names
separate. Provider job evidence does not expose literal YAML job IDs or
structured matrix values, so the assessment must not claim either is available
for this join. Exclude `run_attempt != 1`. Also exclude every candidate key
that has more than one provider job with that same key in one run, recording
`ambiguous-provider-job-identity`; a same-run collision cannot be resolved by
guessing from a YAML ID or matrix value.

### Recommendation calculation and assessment output

For each unambiguous provider key, sample the latest 10 distinct valid,
successful first-attempt jobs completed within the preceding 30 days. Deduplicate
evidence by workflow-run ID, attempt, and provider job ID. A valid sample has
`completed_at - started_at` as a positive duration. Sort durations ascending
and choose nearest-rank p95 at the one-based index `ceil(0.95 * n)`. The
recommended timeout in minutes is:

```
ceil(1.5 * p95_seconds / 60), clamped to 5..120
```

If the unclamped result is over 120 minutes, report
`job-requires-partitioning` rather than presenting a larger allowable timeout.
Fewer than 10 valid samples, stale or missing timestamps, failed, cancelled,
skipped, neutral, or timed-out jobs, non-positive durations, reruns, and
ambiguous identities are explicit advisory evidence gaps. They do not
invalidate an otherwise finite static bound.

For each assessment, output the evidence source URL and retrieval timestamp,
the provider key, sample count/p95 or evidence gaps, the configured timeout,
and the advisory recommended timeout (or `job-requires-partitioning`). The
configured and recommended values are reported together, not asserted equal.
The remediation for coverage failure is
`enforce-ci-timeout-contract`: repair the manifest/workflow composition and
finite bounds using the existing `8ijd` contract. The remediation for an
advisory recommendation is to review and adjust the configured bound or split
an over-120-minute job; it is not an automatic rewrite.

### Anti-patterns (what this criterion must not do)

- Recreate, weaken, or modify the `8ijd` offline manifest/checker, or make
  ordinary CI depend on a live GitHub request.
- Treat unavailable or differing branch-protection/ruleset metadata as a
  static coverage failure instead of
  `required-context-inventory-unverified`.
- Join provider records using invented literal YAML IDs or structured matrix
  values, merge distinct rendered matrix names, or retain same-run key
  collisions and reruns.
- Treat insufficient, stale, or expired runtime evidence as proof that a
  finite offline bound is noncompliant.
- Require checked-in runtime samples, exact configured-versus-recommended
  equality, pre-merge requalification, live PR polling, automatic
  cancellation/rerun, or new status, UI, or health behavior.

---

## Adding new candidates

Follow the structure above:

- Give the criterion a stable `PILLAR-NUMBER` ID.
- Record the source proposal / epic / roadmap provenance.
- Write an unambiguous evidence pattern (what the checker would verify).
- List anti-patterns that would be scored as failures.
- Link the target Agent Readiness proposal (`m2z3`).
