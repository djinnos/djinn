import { beforeEach, describe, expect, it, vi } from "vitest";

import type { Project } from "@/api/server";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";

const mocks = vi.hoisted(() => ({
  createStarterProposal: vi.fn(),
}));

vi.mock("@/api/proposals", () => ({
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

  it("explains the proposal lifecycle and creates a safe targeted draft", async () => {
    const user = userEvent.setup();
    const onFinished = vi.fn();
    render(
      <FirstProposalOnboarding project={project} onFinished={onFinished} />,
      { wrapperOptions: { routerProps: { initialEntries: ["/"] } } },
    );

    const heading = screen.getByRole("heading", {
      level: 1,
      name: "Create your first proposal",
    });
    await waitFor(() => expect(heading).toHaveFocus());
    expect(screen.getByText("Shape")).toBeInTheDocument();
    expect(screen.getByText("Refine")).toBeInTheDocument();
    expect(screen.getByText("Graduate")).toBeInTheDocument();
    expect(screen.getByText("djinnos/example")).toBeInTheDocument();
    expect(
      screen.getByText(/does not start agents or change code/i),
    ).toBeInTheDocument();

    const create = screen.getByRole("button", {
      name: "Create draft proposal",
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
        title: "Reliable draft autosave",
        outcome:
          "Editors keep their latest changes when the network briefly disconnects.",
      }),
    );
    expect(
      await screen.findByRole("heading", {
        name: "Your repository is agent-ready",
      }),
    ).toHaveFocus();
    expect(screen.getByText(/nothing runs yet/i)).toBeInTheDocument();
    expect(screen.getByText(/proposals in the sidebar/i)).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "Open your proposal" }),
    );
    expect(onFinished).toHaveBeenCalledOnce();
  });
});
