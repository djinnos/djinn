# Agent readiness guardrails (S0)

Use this catalog only when the task supplies the exact platform pin
`agent-readiness-guardrails@1.0.0`. Evaluate each applicable composition
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
- **Expected controls:** generated or schema-checked response types, CI drift verification, and render smoke coverage for critical fields.
- **Evidence example:** a server DTO/schema is the source of truth, the client imports its generated type, CI runs the drift check, and a smoke test renders a critical value.
- **Anti-pattern:** hand-maintained optional client mirrors or endpoint tests that omit fields hidden by UI fallbacks.
- **Remediation template:** `protect-api-contract-codegen-drift` — establish a source schema, generation/check command, CI gate, and render smoke.
- **Confidence rule:** high requires both schema/drift proof and rendered-field proof.

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
