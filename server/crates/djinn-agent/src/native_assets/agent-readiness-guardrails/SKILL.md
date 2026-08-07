# Agent readiness guardrails (S0)

Use this catalog only when the task supplies the exact platform pin
`agent-readiness-guardrails@1.2.0`. Evaluate each applicable composition
selector against repository evidence. A guardrail is not satisfied by an
intention, an unrun command, or a path that is unrelated to the delivered
surface.

For every finding record the stable ID, selector, observed evidence, confidence,
and the matching remediation template. `high` confidence needs direct,
current evidence; `medium` needs corroborated indirect evidence; `low` means the
repository could not be inspected sufficiently and must not be reported as a
pass. `not_applicable` needs a concrete stack or surface rationale.

### Guardrail: GOV-MIG-001 — migration immutability
- **Composition selector:** database-backed service with committed migrations.
- **Expected controls:** immutable applied migrations; a CI-enforced migration integrity check; additive follow-up migrations for corrections.
- **Evidence example:** a migration checker is invoked by CI and a negative fixture proves an edited historical migration fails.
- **Anti-pattern:** rewriting an already-applied migration or only documenting the rule without an executable check.
- **Remediation template:** `register-migration-immutability-guard` — add an immutable-history checker, CI invocation, and negative test.
- **Confidence rule:** high only when the check and its CI invocation are both directly verified.

### Guardrail: EVID-ACCEPTANCE-001 — evidence-backed acceptance criteria
- **Composition selector:** tasks or plans that alter behavior.
- **Expected controls:** observable acceptance criteria linked to tests, commands, screenshots, traces, or API responses.
- **Evidence example:** each criterion names a command and expected result with a checked artifact or assertion.
- **Anti-pattern:** “works correctly” or “add tests” without observable proof.
- **Remediation template:** `add-evidence-backed-acceptance-criteria` — replace intentions with criterion, evidence source, and validation command.
- **Confidence rule:** high requires evidence that can be independently rerun or inspected.

### Guardrail: EVID-SMOKE-001 — runnable smoke contracts
- **Composition selector:** runnable application, service, CLI, or UI surface.
- **Expected controls:** documented setup/seed/start steps and a bounded smoke command with an expected health, route, or output assertion.
- **Evidence example:** CI or a documented script starts the surface and checks a real endpoint or headless workflow.
- **Anti-pattern:** a build-only check presented as runtime evidence.
- **Remediation template:** `add-runnable-smoke-contract` — declare prerequisites, start command, smoke action, and expected assertion.
- **Confidence rule:** high requires a recent successful runnable invocation.

### Guardrail: EVID-SELECTOR-001 — stable UI selectors
- **Composition selector:** user-interface behavior with automated interaction or visual evidence.
- **Expected controls:** stable, semantic test selectors at interaction and assertion points, with selector ownership documented.
- **Evidence example:** tests target `data-testid` or an equivalent stable accessibility contract rather than layout or incidental copy.
- **Anti-pattern:** CSS classes, nth-child paths, or volatile display text as the only selector.
- **Remediation template:** `add-stable-ui-selectors` — add owned selectors and update smoke/evidence tests to use them.
- **Confidence rule:** medium or lower if only selectors exist but no test uses them.

### Guardrail: CONTRACT-API-001 — API-contract/type-codegen drift protection
- **Composition selector:** UI or client consumes typed API responses.
- **Expected controls:** a source schema; generated or schema-checked response types; one repository-local executable generation/update command that refreshes every derived artifact of that schema; CI drift verification whose failure output names that command; and render smoke coverage for critical fields.
- **Evidence example:** readiness cites the source schema, the generated client type import, the command definition, the drift-check definition and its failure hint, the CI invocation, and a critical-field render smoke test. A `Makefile` target, package script, checked-in script, or CI/test source is acceptable repository evidence; the command does not have to be executed to determine catalog compliance.
- **Anti-pattern:** a drift check exists but authors must discover or manually sequence the regeneration steps themselves; the failure output does not name the command; hand-maintained optional client mirrors or endpoint tests hide fields missing behind UI fallbacks.
- **Remediation template:** `protect-api-contract-codegen-drift` — establish the source schema, one generation/update command, a check that names that command on failure, a CI gate, and render smoke.
- **Confidence rule:** high requires repository evidence for the command and its named failure hint, plus schema/drift proof and rendered-field proof. If either the command or the named failure hint is absent, the control is noncompliant — not high or medium confidence.

### Guardrail: TEST-DB-001 — DB-backed integration tests
- **Composition selector:** behavior persists, queries, migrates, or enforces database constraints.
- **Expected controls:** integration tests against a real disposable database and migrations, with isolation and deterministic fixtures.
- **Evidence example:** a test applies migrations then verifies a repository or endpoint behavior against the database.
- **Anti-pattern:** mocks-only coverage for SQL, migration, transaction, or constraint behavior.
- **Remediation template:** `add-db-backed-integration-tests` — provision the test database, migrate it, add isolated fixtures, and assert real behavior.
- **Confidence rule:** high only when the test uses the production database dialect and migration path.

### Guardrail: OBS-BASELINE-001 — observability baseline
- **Composition selector:** long-running service, job, queue consumer, or API.
- **Expected controls:** structured logs, bounded-label metrics, error context, and a documented signal for the critical operation.
- **Evidence example:** an integration or unit test verifies emitted operation, outcome, and bounded metric dimensions.
- **Anti-pattern:** unstructured logs with no request/job identity or metrics using unbounded user input as labels.
- **Remediation template:** `establish-observability-baseline` — instrument operation/outcome/error context and add bounded metric/log assertions.
- **Confidence rule:** high requires code evidence plus a test or verified runtime signal.

### Guardrail: SUPPLY-CHAIN-001 — dependency and supply-chain policy
- **Composition selector:** repository builds or ships third-party dependencies.
- **Expected controls:** lockfiles, dependency update/review policy, vulnerability or provenance checks appropriate to the ecosystem, and CI enforcement.
- **Evidence example:** CI validates the lockfile and runs the declared audit or provenance policy with an owned remediation path.
- **Anti-pattern:** unpinned dependencies, ignored audit output, or a policy document that CI never executes.
- **Remediation template:** `enforce-dependency-supply-chain-policy` — pin the resolved graph, add the ecosystem check to CI, and define remediation owners.
- **Confidence rule:** high requires current CI execution; stale documentation is low confidence.


### Guardrail: CI-TIMEOUT-001 — CI timeout coverage and evidence-based recommendations
- **Composition selector:** a checked-in v1 `.github/ci-timeouts.json` manifest. Its `terminalRoots` select terminal CI compositions and its `covered` identities declare the complete resolved composition. Canonical identities are `.github/workflows/<workflow>.yml#<job>`, with `=>` prefixes for reusable-workflow call chains.
- **Expected controls:** retain `.github/ci-timeouts.json` and run `scripts/check-ci-timeouts.mjs` as the stable offline, fail-closed evidence source. The checker expands `needs` and local reusable workflows from every terminal root, validates the sorted manifest inventory, and requires finite `timeout-minutes` bounds transitively. When available, compare manifest terminal-context declarations with the authoritative protected-target-branch required-context inventory from GitHub branch-protection required-status checks and applicable repository rulesets.
- **Coverage-compliance outcome:** report the local manifest/checker result separately as stable coverage compliance. Missing branch-protection/ruleset metadata, or metadata different from the manifest declarations, reports `required-context-inventory-unverified`; it is not a substitute for the offline checker's result. Runtime-evidence freshness, GitHub availability, and expired source URLs cannot fail or overturn the offline checker or an otherwise finite static bound.
- **Recommendation-confidence outcome:** report runtime evidence quality separately as advisory recommendation confidence. It can report a recommendation when sufficient evidence exists or explicit evidence gaps when it does not; neither outcome changes coverage compliance.
- **Runtime assessment procedure:** obtain workflow metadata with `GET /repos/{owner}/{repo}/actions/workflows`, workflow runs with `GET /repos/{owner}/{repo}/actions/workflows/{workflow_id}/runs`, and attempt-1 jobs with `GET /repos/{owner}/{repo}/actions/runs/{run_id}/attempts/1/jobs`. Use the provider-exposed key exactly `(workflow path, exact rendered job name, normalized/sorted runner labels, runner_group_id)`. Do not claim provider evidence supplies literal YAML job IDs or structured matrix values. Keep distinct rendered matrix names separate, exclude `run_attempt != 1`, and exclude every same-run duplicate of that key as `ambiguous-provider-job-identity` rather than guessing a join.
- **Recommendation calculation:** for each unambiguous provider key, deduplicate by `(workflow-run ID, attempt, provider job ID)`, then use the latest 10 distinct valid successful first-attempt jobs completed within 30 days. A valid sample has a positive duration `completed_at - started_at`; exclude stale or missing timestamps, failed, cancelled, skipped, neutral, and timed-out jobs, reruns, non-positive durations, and ambiguous identities, recording each as an explicit advisory evidence gap. Sort durations ascending and select nearest-rank p95 at one-based `ceil(0.95 * n)`. Recommend `ceil(1.5 * p95_seconds / 60)` minutes, clamped to `5..120`; if the unclamped result exceeds 120, report `job-requires-partitioning` rather than a larger allowable timeout. Fewer than 10 valid samples is an advisory evidence gap, not static noncompliance.
- **Evidence example:** record source URL and retrieval timestamp, provider key, sample count and p95 or evidence gaps, configured timeout, and recommended timeout (or `job-requires-partitioning`) together. Do not require configured and recommended values to be equal.
- **Anti-pattern:** recreating or weakening the offline contract; making ordinary CI depend on a live GitHub request; treating missing/different authoritative metadata, stale runtime evidence, or URL expiry as coverage failure; merging rendered matrix names; or retaining reruns/collisions by inventing YAML IDs or matrix data.
- **Remediation template:** `enforce-ci-timeout-contract` — repair the manifest/workflow composition and finite bounds through the existing offline contract. For advisory findings, review and adjust the configured bound or split a `job-requires-partitioning` job; do not automatically rewrite, cancel, or rerun CI.
- **Confidence rule:** high recommendation confidence requires 10 valid, unambiguous samples per provider key within 30 days and cited source URLs/timestamps. Coverage confidence remains governed by the offline checker independently of runtime evidence.
