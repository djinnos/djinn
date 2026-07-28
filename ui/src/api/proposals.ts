import { callMcpTool } from "@/api/mcpClient";
import type { Project } from "@/api/server";

export type StarterProposalKind = "agentic-ready" | "custom";

export interface StarterProposalInput {
  project: Project;
  kind?: StarterProposalKind;
  title: string;
  outcome: string;
}

export interface CreatedStarterProposal {
  id: string;
  shortId: string | null;
  title: string;
}

export const AGENTIC_READY_TITLE =
  "Make the development environment agent-ready";

export const AGENTIC_READY_OUTCOME =
  "A developer or agent can start from a clean checkout and run the documented setup, build, lint, and test workflows deterministically, with CI parity and no undocumented manual steps.";

export async function hasAnyProposal(): Promise<boolean> {
  const response = await callMcpTool("proposal_list", {
    sort: "created_desc",
    limit: 1,
  });
  return (response.proposals?.length ?? 0) > 0;
}

export async function createStarterProposal({
  project,
  kind = "custom",
  title,
  outcome,
}: StarterProposalInput): Promise<CreatedStarterProposal> {
  const repository = `${project.github_owner}/${project.github_repo}`;
  const isAgenticReady = kind === "agentic-ready";
  const response = await callMcpTool("proposal_create", {
    title: title.trim(),
    body: isAgenticReady
      ? agenticReadyProposalBody(repository)
      : starterProposalBody(outcome.trim(), repository),
    body_format: "markdown",
    status: "draft",
    target_projects: [project.id],
    acceptance_criteria: isAgenticReady
      ? [
          "A clean checkout can run documented, non-interactive setup, build, lint, and test commands.",
          "CI and the local or agent runtime use compatible, pinned toolchain versions.",
          "Fast validation and the full validation path are deterministic and clearly documented.",
          "Required services, secrets, and external dependencies are explicit and have safe local or test defaults.",
        ]
      : [
          "The current behavior and affected code paths are documented before implementation.",
          "The proposed change has deterministic automated validation.",
          "The implementation and rollback path are clear enough for human review.",
        ],
  });

  if (response.error) throw new Error(response.error);
  if (!response.id) throw new Error("Djinn created the proposal without returning its ID");

  return {
    id: response.id,
    shortId: response.short_id ?? null,
    title: response.title ?? title.trim(),
  };
}

export function starterProposalBody(outcome: string, repository: string): string {
  return `## Outcome

${outcome}

## Context

This is the first proposal for \`${repository}\`. It gives Djinn a bounded,
reviewable outcome before any implementation work begins.

## Scope

- Inspect the current behavior and the code paths that own it.
- Design the smallest safe change that achieves the outcome.
- Identify dependencies, failure modes, and rollout or rollback concerns.
- Define deterministic validation before the proposal graduates into tasks.

## Non-goals

- Do not start implementation while this proposal is still a draft.
- Do not broaden the change beyond the stated outcome without explicit review.

## Validation

- The proposal names concrete entry points and affected components.
- Acceptance criteria are testable and unambiguous.
- A human can review the plan before agents are allowed to execute it.`;
}

export function agenticReadyProposalBody(repository: string): string {
  return `## Outcome

A developer or agent can start from a clean checkout of \`${repository}\` and
run the documented setup, build, lint, and test workflows deterministically,
with CI parity and no undocumented manual steps.

## Current-state audit

- Map the existing CI workflows, package scripts, toolchain versions, and caches.
- Identify required services, secrets, fixtures, and external dependencies.
- Record flaky checks, hidden setup steps, weak failure output, and local/CI drift.

## Scope

- Establish canonical, non-interactive setup, build, lint, and test commands.
- Pin and align toolchain versions across local development, the agent image, and CI.
- Make CI execute the same or compatible validation paths used by developers and agents.
- Isolate flaky or external dependencies and provide safe defaults for required services and secrets.
- Make timeouts, logs, and failures actionable enough for an agent or developer to diagnose.
- Document the fast feedback loop and the full pre-merge validation path.

## Non-goals

- Do not add unrelated product features.
- Do not commit provider credentials or secrets to the repository.
- Do not hide failures by disabling tests, lint rules, or required checks.
- Do not start implementation while this proposal is still a draft.

## Validation

- A clean checkout in the selected agent image can follow the documented setup and validation path.
- CI runs the same or compatible commands with deterministic results.
- No undocumented interactive or manual step is required.
- Failures identify the broken command, dependency, or environment assumption.`;
}
