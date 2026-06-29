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

## Adding new candidates

Follow the structure above:

- Give the criterion a stable `PILLAR-NUMBER` ID.
- Record the source proposal / epic / roadmap provenance.
- Write an unambiguous evidence pattern (what the checker would verify).
- List anti-patterns that would be scored as failures.
- Link the target Agent Readiness proposal (`m2z3`).
