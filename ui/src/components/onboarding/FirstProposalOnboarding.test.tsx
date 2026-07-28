import { beforeEach, describe, expect, it, vi } from "vitest";

import type { Project } from "@/api/server";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

const mocks = vi.hoisted(() => ({
  createStarterProposal: vi.fn(),
  startStarterProposalRefinement: vi.fn(),
}));

vi.mock("@/api/proposals", () => ({
  AGENTIC_READY_OUTCOME:
    "A developer or agent can start from a clean checkout and run the documented setup, build, lint, and test workflows deterministically, with CI parity and no undocumented manual steps.",
  AGENTIC_READY_TITLE: "Make the development environment agent-ready",
  createStarterProposal: mocks.createStarterProposal,
  startStarterProposalRefinement: mocks.startStarterProposalRefinement,
}));

import { FirstProposalOnboarding } from "./FirstProposalOnboarding";

const project = {
  id: "project-1",
  name: "Example",
  github_owner: "djinnos",
  github_repo: "example",
} as Project;

describe("FirstProposalOnboarding", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.createStarterProposal.mockResolvedValue({
      id: "proposal-1",
      shortId: "PROP-1",
      title: "Reliable draft autosave",
      refinementStarted: true,
      refinementError: null,
    });
    mocks.startStarterProposalRefinement.mockResolvedValue(undefined);
  });

  it("defaults to a safe agentic-ready environment proposal", async () => {
    const user = userEvent.setup();
    const onFinished = vi.fn();
    render(
      <FirstProposalOnboarding project={project} onFinished={onFinished} />,
      { wrapperOptions: { routerProps: { initialEntries: ["/"] } } },
    );

    const heading = screen.getByRole("heading", {
      level: 1,
      name: "Choose your first proposal",
    });
    await waitFor(() => expect(heading).toHaveFocus());
    expect(screen.getByText("Shape")).toBeInTheDocument();
    expect(screen.getByText("Refine")).toBeInTheDocument();
    expect(screen.getByText("Graduate")).toBeInTheDocument();
    expect(screen.getByText("djinnos/example")).toBeInTheDocument();
    expect(
      screen.getByText(/automatically starts the refinement tribunal/i),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Agentic-ready environment/i }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(
      screen.getByRole("button", { name: /Custom proposal/i }),
    ).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByLabelText("What should change?")).not.toBeInTheDocument();
    expect(screen.getByText("CI parity")).toBeInTheDocument();

    const create = screen.getByRole("button", {
      name: "Create agent-ready draft",
    });
    expect(create).toBeEnabled();
    await user.click(create);

    await waitFor(() =>
      expect(mocks.createStarterProposal).toHaveBeenCalledWith({
        project,
        kind: "agentic-ready",
        title: "Make the development environment agent-ready",
        outcome:
          "A developer or agent can start from a clean checkout and run the documented setup, build, lint, and test workflows deterministically, with CI parity and no undocumented manual steps.",
      }),
    );
    expect(
      await screen.findByRole("heading", {
        name: "Draft created · refinement started",
      }),
    ).toHaveFocus();
    expect(screen.getByText(/tribunal is refining it automatically/i)).toBeInTheDocument();
    expect(screen.getByText(/proposal tour opens/i)).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Open your proposal" }),
    );
    expect(onFinished).toHaveBeenCalledOnce();
  });

  it("retries refinement on the existing proposal without creating a duplicate", async () => {
    const user = userEvent.setup();
    mocks.createStarterProposal.mockResolvedValueOnce({
      id: "proposal-1",
      shortId: "PROP-1",
      title: "Agent-ready environment",
      refinementStarted: false,
      refinementError: "No plan model is available",
    });
    render(
      <FirstProposalOnboarding project={project} onFinished={vi.fn()} />,
      { wrapperOptions: { routerProps: { initialEntries: ["/"] } } },
    );

    await user.click(
      screen.getByRole("button", { name: "Create agent-ready draft" }),
    );
    expect(
      await screen.findByRole("heading", {
        name: "Draft created · refinement needs attention",
      }),
    ).toHaveFocus();
    expect(screen.getByText("No plan model is available")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Retry refinement" }));
    await waitFor(() =>
      expect(mocks.startStarterProposalRefinement).toHaveBeenCalledWith(
        "proposal-1",
      ),
    );
    expect(mocks.createStarterProposal).toHaveBeenCalledTimes(1);
    expect(
      await screen.findByRole("heading", {
        name: "Draft created · refinement started",
      }),
    ).toBeInTheDocument();
  });

  it("keeps the flexible outcome form as the custom path", async () => {
    const user = userEvent.setup();
    render(
      <FirstProposalOnboarding project={project} onFinished={vi.fn()} />,
      { wrapperOptions: { routerProps: { initialEntries: ["/"] } } },
    );

    await user.click(
      screen.getByRole("button", { name: /Custom proposal/i }),
    );
    expect(
      screen.getByRole("button", { name: /Custom proposal/i }),
    ).toHaveAttribute("aria-pressed", "true");
    const create = screen.getByRole("button", {
      name: "Create custom draft",
    });
    expect(create).toBeDisabled();

    await user.type(
      screen.getByLabelText("What should change?"),
      "Reliable draft autosave",
    );
    await user.type(
      screen.getByLabelText("Desired outcome"),
      "Editors keep their latest changes when the network briefly disconnects.",
    );
    await user.click(create);

    await waitFor(() =>
      expect(mocks.createStarterProposal).toHaveBeenCalledWith({
        project,
        kind: "custom",
        title: "Reliable draft autosave",
        outcome:
          "Editors keep their latest changes when the network briefly disconnects.",
      }),
    );
  });
});
