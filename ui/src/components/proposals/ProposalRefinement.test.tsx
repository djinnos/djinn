import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@/test/test-utils";
import type { ProposalRefinementStatus } from "@/api/types";
import { ProposalRefinement } from "./ProposalRefinement";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/lib/toast", () => ({
  showToast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

describe("ProposalRefinement", () => {
  const proposalId = "test-proposal-id";

  it("renders nothing when status is null and canStart is false", () => {
    const { container } = render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(container.innerHTML).toBe("");
  });

  it("renders kickoff affordance when canStart is true and no status", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Proposal refinement")).toBeInTheDocument();
    expect(screen.getByText("Start refinement")).toBeInTheDocument();
    expect(screen.getByText("Checkpoint")).toBeInTheDocument();
  });

  it("renders active status panel", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 3,
      dry_rounds: 1,
      total_entries: 7,
      update_authority: "checkpoint",
      stop_reason: null,
      pending_checkpoint_count: 0,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.getByText("Checkpoint")).toBeInTheDocument();
    expect(screen.getByText("Round")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
    expect(screen.getByText("Entries")).toBeInTheDocument();
    expect(screen.getByText("7")).toBeInTheDocument();
    expect(screen.getByText("Dry rounds")).toBeInTheDocument();
    expect(screen.getByText("1")).toBeInTheDocument();
  });

  it("renders stopped status panel with stop reason", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 5,
      dry_rounds: 2,
      total_entries: 12,
      update_authority: "auto_accept",
      stop_reason: "adversary_dry",
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Stopped")).toBeInTheDocument();
    expect(screen.getByText("Auto-accept")).toBeInTheDocument();
    expect(
      screen.getByText(/Adversary exhausted/),
    ).toBeInTheDocument();
  });

  it("explains same-model fallback in the status panel", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 0,
      update_authority: "checkpoint",
      stop_reason: null,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/best-effort cross-model diversity/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/falls back to the same model/),
    ).toBeInTheDocument();
  });

  it("explains checkpoint vs auto-accept in kickoff affordance", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/advocate revisions are proposed for approval/),
    ).toBeInTheDocument();
  });

  it("renders pending checkpoint count in checkpoint mode", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      update_authority: "checkpoint",
      stop_reason: null,
      pending_checkpoint_count: 2,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/2 advocate revision\(s\) awaiting approval/)).toBeInTheDocument();
  });

  it("does not show pending revisions in auto-accept mode", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      update_authority: "auto_accept",
      stop_reason: null,
      pending_checkpoint_count: 0,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.queryByText(/pending revision/)).not.toBeInTheDocument();
  });

  it("shows checkpoint approval message when no pending revisions", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 0,
      update_authority: "checkpoint",
      stop_reason: null,
      pending_checkpoint_count: 0,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/Advocate revisions require explicit approval/),
    ).toBeInTheDocument();
  });
});
