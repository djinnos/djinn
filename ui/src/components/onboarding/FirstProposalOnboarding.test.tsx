import { beforeEach, describe, expect, it, vi } from "vitest";

import type { Project } from "@/api/server";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

const mocks = vi.hoisted(() => ({
  createStarterProposal: vi.fn(),
}));

vi.mock("@/api/proposals", () => ({
  AGENTIC_READY_OUTCOME:
    "A developer or agent can start from a clean checkout and run the documented setup, build, lint, and test workflows deterministically, with CI parity and no undocumented manual steps.",
  AGENTIC_READY_TITLE: "Make the development environment agent-ready",
  createStarterProposal: mocks.createStarterProposal,
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
    });
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
      screen.getByText(/does not start agents or change code/i),
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
        name: "Your first proposal is ready",
      }),
    ).toHaveFocus();
    expect(screen.getByText(/nothing runs yet/i)).toBeInTheDocument();
    expect(screen.getByText(/proposals in the sidebar/i)).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Open your proposal" }),
    );
    expect(onFinished).toHaveBeenCalledOnce();
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
