import { callMcpTool } from "@/api/mcpClient";
import type { Project } from "@/api/server";

export interface StarterProposalInput {
  project: Project;
  title: string;
  outcome: string;
}

export interface CreatedStarterProposal {
  id: string;
  shortId: string | null;
  title: string;
}

export async function hasAnyProposal(): Promise<boolean> {
  const response = await callMcpTool("proposal_list", {
    sort: "created_desc",
    limit: 1,
  });
  return (response.proposals?.length ?? 0) > 0;
}

export async function createStarterProposal({
  project,
  title,
  outcome,
}: StarterProposalInput): Promise<CreatedStarterProposal> {
  const repository = `${project.github_owner}/${project.github_repo}`;
  const response = await callMcpTool("proposal_create", {
    title: title.trim(),
    body: starterProposalBody(outcome.trim(), repository),
    body_format: "markdown",
    status: "draft",
    target_projects: [project.id],
    acceptance_criteria: [
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
