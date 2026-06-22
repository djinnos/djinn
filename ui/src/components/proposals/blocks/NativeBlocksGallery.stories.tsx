import type { Meta, StoryObj } from "@storybook/react-vite";

import { BlockRenderer } from "./BlockRenderer";

/**
 * Native-blocks review gallery.
 *
 * Renders a complete, realistic proposal — exercising EVERY proposal block type
 * in document context — through the real {@link BlockRenderer}. This is the
 * surface used to judge the "feel native, not boxed in grey blocks" redesign:
 * the structured blocks (diagram, files, decisions, open questions, data model,
 * api, tabs, …) should read as part of one intentional document, while genuine
 * code surfaces (code, diff) keep their frame.
 *
 * Open this story in Storybook (Proposals/Native Blocks Gallery) and scroll the
 * single page to review each block in place.
 */
const meta = {
  title: "Proposals/Native Blocks Gallery",
  parameters: { layout: "fullscreen" },
} satisfies Meta;

export default meta;

// A literal backtick. We build the body with `String.raw` so the JSON in
// container attributes (`tabs={…}`) keeps its valid escapes (`\n`, `\"`)
// untouched; injecting backticks via `${bt}` keeps real markdown inline-code
// fences in the text WITHOUT introducing an invalid `\`` JSON escape that would
// make the tabs attribute fail to parse.
const bt = "`";

// A full proposal body modelled on a real djinn "CI guardrails" spec, padded out
// so every block type appears at least once with realistic, varied content.
const PROPOSAL_BODY = String.raw`# Problem

The workspace has **no ${bt}cargo-deny${bt}** at all — there is no ${bt}deny.toml${bt} at the repo root or under ${bt}server/${bt}, and no ${bt}cargo deny${bt} step in ${bt}.github/workflows/${bt}. That means **zero CVE / yanked-crate / license scanning** today.

Meanwhile we already _built_ an internal architectural-boundary checker — ${bt}server/ci/check_boundaries.rs${bt} + ${bt}server/boundary_rules.toml${bt} — but it is **not wired into the GitHub quality gate**; it only runs in djinn's own warm pipeline.

This proposal closes those gaps. It is **CI/config only** — it changes no Rust source logic, so it cannot conflict with the parallel code proposals.

## Target guardrail layers

<Diagram id="arch-overview" type="mermaid">
flowchart LR
  PR[PR push] --> A[clippy + fmt]
  A --> B[hakari verify]
  B --> C[cargo-deny: advisories + licenses]
  C --> D[check_boundaries.rs]
  D --> E[sqlx prepare --check]
  E --> M{merge_group}
</Diagram>

<Callout id="scope-note" tone="info">
**CI/config only.** This proposal touches ${bt}server/deny.toml${bt}, ${bt}.github/workflows/quality-gate.yml${bt}, root ${bt}server/Cargo.toml${bt}, ${bt}scripts/${bt}, and (optionally) the ${bt}check_boundaries.rs${bt} CI wiring — **no Rust source-logic changes**.
</Callout>

## The three guardrails

<Tabs id="guardrails" tabs={[{ "label": "cargo-deny supply-chain", "body": "Add **cargo-deny** with advisories (deny vulnerabilities + unmaintained/yanked) and licenses (explicit allowlist) as a NEW CI job mirroring the existing **hakari** job structure.\n\n<AnnotatedCode id=\"deny-toml\" filename=\"server/deny.toml\" language=\"toml\" annotations={[{\"lines\":\"1-2\",\"note\":\"Yanked + vulnerable crates fail the build.\"},{\"lines\":\"5\",\"note\":\"Finalize the allowlist during rollout — start permissive, then tighten.\"}]}>\n[advisories]\nyanked = \"deny\"\nvulnerability = \"deny\"\n[licenses]\nallow = [\"MIT\", \"Apache-2.0\", \"BSD-3-Clause\", \"ISC\"]\n</AnnotatedCode>\n\n<Callout id=\"deny-warn\" tone=\"warning\">\nThe first run **will** surface existing advisories. Triage them rather than blocking the rollout PR.\n</Callout>" }, { "label": "boundary checker", "body": "Wire **check_boundaries.rs** into the quality gate so leaf-isolation rules (e.g. no-agent-imports-db) are enforced on every PR, not just in the warm pipeline.\n\n<Diff id=\"gate-diff\" filename=\".github/workflows/quality-gate.yml\" lang=\"yaml\">\n@@ -18,6 +18,11 @@ jobs:\n     - name: hakari verify\n       run: cargo hakari generate --diff\n+    - name: cargo-deny\n+      run: cargo deny check advisories licenses\n+    - name: boundary check\n+      run: cargo run -p ci --bin check_boundaries\n     - name: clippy\n       run: cargo clippy --all-features\n</Diff>" }, { "label": "sqlx freshness", "body": "Move the **sqlx prepare --check** step from the merge_group-only stage to **PR push**, so a stale .sqlx cache is caught early.\n\n<Checklist id=\"sqlx-steps\">\n- [x] Add prepare --check to PR job\n- [x] Document regen in CONTRIBUTING\n- [ ] Backfill .sqlx for the three lagging crates\n</Checklist>" }]} />

## Affected files

<FileTree id="affected" root="djinn">
server/
  deny.toml +
  Cargo.toml ~
  ci/
    check_boundaries.rs +
    boundary_rules.toml
.github/workflows/
  quality-gate.yml ~
scripts/
  verify.sh ~
  - legacy-audit.sh
</FileTree>

## Decisions

<Decisions id="adrs">
### Enforce cargo-deny on PR push, not merge_group
Status: accepted

Context
Catching a yanked/vulnerable crate at merge-queue time wastes a full CI cycle and blocks the queue for everyone.

Decision
Run cargo deny check on every PR push, gated as a required check.

Consequences
Slightly longer PR CI (~40s); first run needs an advisory triage pass.

### Block on license violations
Status: accepted

Use an explicit allowlist; an unlisted license fails the build rather than warning.

### Vendor the advisory DB
Status: rejected

Operationally heavy; the hosted advisory DB fetch is fast enough and cached.

### Gate boundaries via the warm pipeline only
Status: superseded by #2

Earlier direction before we decided to wire the checker straight into the gate.
</Decisions>

## Data shapes

The ${bt}DenyConfig${bt} document the gate reads:

| field | type | notes |
| --- | --- | --- |
| advisories.yanked | enum(deny,warn,allow) | required |
| advisories.ignore | string[] | advisory IDs to skip |
| licenses.allow | string[] | SPDX identifiers |
| licenses.confidence | float | default 0.8 |

## Reporting endpoint

<ApiEndpoint id="report" method="POST" path="/api/ci/deny-report">
Uploads a cargo-deny JSON report for a PR run. Requires a bearer token.

## Parameters
| name | in | type | required | description |
| --- | --- | --- | --- | --- |
| pr | query | integer | true | The PR number |
| sha | body | string | true | Head commit SHA |

## Responses
| status | description |
| --- | --- |
| 202 | Report accepted |
| 401 | Missing/invalid token |
</ApiEndpoint>

## Sample config payload

<JsonExplorer id="sample-payload">
{
  "advisories": { "yanked": "deny", "ignore": ["RUSTSEC-2021-0127"] },
  "licenses": { "allow": ["MIT", "Apache-2.0"], "confidence": 0.8 },
  "targets": ["x86_64-unknown-linux-gnu"],
  "enabled": true
}
</JsonExplorer>

## All callout tones

<Callout id="c-info" tone="info">**Info** — heads-up context that isn't a warning.</Callout>
<Callout id="c-decision" tone="decision">**Decision** — a settled choice worth flagging inline.</Callout>
<Callout id="c-risk" tone="risk">**Risk** — a sharp edge reviewers must weigh.</Callout>
<Callout id="c-success" tone="success">**Success** — a guarantee or invariant that now holds.</Callout>

## Side-by-side

<Columns id="before-after" columns={[{ "body": "### Before\n- No supply-chain scan\n- Boundaries warm-only\n- sqlx checked late" }, { "body": "### After\n- cargo-deny on every PR\n- Boundaries gate-enforced\n- sqlx checked on push" }]} />

## Settings mock

<Wireframe id="settings-ui" surface="browser">
┌────────────────────────────────────────────┐
│  CI guardrails                      [ x ]  │
├────────────────────────────────────────────┤
│                                            │
│  Repository                                │
│  ┌───────────────────────────────────────┐ │
│  │ djinnos/djinn                         │ │
│  └───────────────────────────────────────┘ │
│                                            │
│  [x] cargo-deny advisories                 │
│  [x] license allowlist                     │
│  [ ] boundary checker                      │
│                                            │
│         ┌──────────┐  ┌──────────┐         │
│         │  Cancel  │  │   Save   │         │
│         └──────────┘  └──────────┘         │
│                                            │
└────────────────────────────────────────────┘
</Wireframe>

## Acceptance criteria

<Checklist id="acceptance">
- [x] server/deny.toml committed
- [x] cargo-deny step added to quality-gate.yml
- [ ] boundary checker wired and green
- [ ] advisory triage doc linked from CONTRIBUTING
</Checklist>

<QuestionForm id="open-questions" title="Open Questions">
Should the license allowlist live in deny.toml or a separate licenses.toml?
- Single file is simpler to review _(recommended)_
- Separate file isolates churn

Do we block on unmaintained advisories or only vulnerability advisories?

Who owns the advisory triage rotation?
</QuestionForm>

Some trailing markdown to confirm the document reads continuously to the end.`;

export const FullProposal: StoryObj = {
  render: () => (
    <div className="min-h-screen bg-background text-foreground">
      <div className="mx-auto max-w-3xl space-y-6 p-6">
        <div className="text-[11px] font-semibold uppercase tracking-wider text-muted-foreground">
          Spec
        </div>
        <BlockRenderer body={PROPOSAL_BODY} />
      </div>
    </div>
  ),
};
