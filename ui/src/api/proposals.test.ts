import { beforeEach, describe, expect, it, vi } from "vitest";

import { callMcpTool } from "@/api/mcpClient";
import type { Project } from "@/api/server";

import {
  AGENTIC_READY_OUTCOME,
  AGENTIC_READY_TITLE,
  agenticReadyProposalBody,
  createStarterProposal,
  hasAnyProposal,
  starterProposalBody,
} from "./proposals";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

const project = {
  id: "project-1",
  name: "Example",
  github_owner: "djinnos",
  github_repo: "example",
} as Project;

describe("proposal onboarding API", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
  });

  it("checks readiness with the smallest newest-first proposal query", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      proposals: [{ id: "proposal-1" }],
    } as never);

    await expect(hasAnyProposal()).resolves.toBe(true);
    expect(callMcpTool).toHaveBeenCalledWith("proposal_list", {
      sort: "created_desc",
      limit: 1,
    });
  });

  it("creates a targeted draft without starting implementation", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      id: "proposal-1",
      short_id: "PROP-1",
      title: "Reliable draft autosave",
    } as never);

    await expect(
      createStarterProposal({
        project,
        title: "  Reliable draft autosave  ",
        outcome:
          "Editors keep their latest changes when the network briefly disconnects.",
      }),
    ).resolves.toEqual({
      id: "proposal-1",
      shortId: "PROP-1",
      title: "Reliable draft autosave",
    });

    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_create",
      expect.objectContaining({
        title: "Reliable draft autosave",
        body_format: "markdown",
        status: "draft",
        target_projects: ["project-1"],
        acceptance_criteria: expect.arrayContaining([
          expect.stringMatching(/automated validation/i),
        ]),
      }),
    );
    const body = (
      vi.mocked(callMcpTool).mock.calls[0]?.[1] as
        | { body?: string }
        | undefined
    )?.body;
    expect(body).toContain("## Outcome");
    expect(body).toContain("## Non-goals");
    expect(body).toContain("Do not start implementation");
  });

  it("creates a concrete agentic-ready environment draft", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      id: "proposal-agentic",
      short_id: "PROP-2",
      title: AGENTIC_READY_TITLE,
    } as never);

    await createStarterProposal({
      project,
      kind: "agentic-ready",
      title: AGENTIC_READY_TITLE,
      outcome: AGENTIC_READY_OUTCOME,
    });

    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_create",
      expect.objectContaining({
        title: AGENTIC_READY_TITLE,
        status: "draft",
        target_projects: ["project-1"],
        acceptance_criteria: expect.arrayContaining([
          expect.stringMatching(/clean checkout/i),
          expect.stringMatching(/CI.*pinned toolchain/i),
          expect.stringMatching(/services, secrets/i),
        ]),
      }),
    );
    const body = (
      vi.mocked(callMcpTool).mock.calls[0]?.[1] as
        | { body?: string }
        | undefined
    )?.body;
    expect(body).toContain("## Current-state audit");
    expect(body).toContain("## Non-goals");
    expect(body).toContain("Do not hide failures");
    expect(body).toContain("No undocumented interactive or manual step");
  });

  it("makes the safety boundary explicit in the starter body", () => {
    expect(
      starterProposalBody("A bounded outcome.", "djinnos/example"),
    ).toMatch(/human can review the plan before agents are allowed to execute/i);
    expect(agenticReadyProposalBody("djinnos/example")).toMatch(
      /Do not start implementation while this proposal is still a draft/i,
    );
  });
});
