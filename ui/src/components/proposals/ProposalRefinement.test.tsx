import { describe, expect, it, vi } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import type { ProposalRefinementStatus } from "@/api/types";
import { ProposalRefinement } from "./ProposalRefinement";
import { callMcpTool } from "@/api/mcpClient";
import { showToast } from "@/lib/toast";

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

  // ── All stop reasons ─────────────────────────────────────────────────────

  it("renders round_cap stop reason label", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 5,
      dry_rounds: 0,
      total_entries: 15,
      update_authority: "auto_accept",
      stop_reason: "round_cap",
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
    expect(screen.getByText(/Round cap reached/)).toBeInTheDocument();
  });

  it("renders spawn_cap stop reason label", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 3,
      dry_rounds: 0,
      total_entries: 8,
      update_authority: "checkpoint",
      stop_reason: "spawn_cap",
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
    expect(screen.getByText(/Agent spawn cap reached/)).toBeInTheDocument();
  });

  it("renders repeated_objection stop reason label", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 4,
      dry_rounds: 0,
      total_entries: 10,
      update_authority: "auto_accept",
      stop_reason: "repeated_objection",
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
    expect(screen.getByText(/Repeated objection detected/)).toBeInTheDocument();
  });

  it("renders agent_failure stop reason label", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 3,
      update_authority: "checkpoint",
      stop_reason: "agent_failure",
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
    expect(screen.getByText(/Agent failure/)).toBeInTheDocument();
  });

  it("renders unknown stop reason as-is", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 1,
      update_authority: "checkpoint",
      stop_reason: "some_future_reason",
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
    expect(screen.getByText(/some_future_reason/)).toBeInTheDocument();
  });

  // ── Status refresh via onChanged ─────────────────────────────────────────

  it("calls onChanged after successful start refinement", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      proposal_id: proposalId,
      refinement: { active: true, update_authority: "checkpoint" },
    } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByText("Start refinement"));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_start", {
      proposal_id: proposalId,
      update_authority: "checkpoint",
    });
    expect(showToast.success).toHaveBeenCalledWith("Refinement started");
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("does not call onChanged when start refinement fails", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      error: "proposal not found",
    } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByText("Start refinement"));

    expect(showToast.error).toHaveBeenCalledWith(
      "Failed to start refinement",
      { description: "proposal not found" },
    );
    expect(onChanged).not.toHaveBeenCalled();
  });

  // ── Pending checkpoint controls ──────────────────────────────────────────

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

  it("does not show checkpoint explanation in auto-accept mode", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 3,
      update_authority: "auto_accept",
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
      screen.queryByText(/Advocate revisions require explicit approval/),
    ).not.toBeInTheDocument();
  });

  it("does not show checkpoint explanation when stopped", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 5,
      dry_rounds: 2,
      total_entries: 12,
      update_authority: "checkpoint",
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
    // Checkpoint explanation only shown for active + checkpoint mode.
    expect(
      screen.queryByText(/Advocate revisions require explicit approval/),
    ).not.toBeInTheDocument();
  });

  // ── Auto-accept kickoff mode ─────────────────────────────────────────────

  it("shows auto-accept explanation when auto_accept mode is selected", async () => {
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={vi.fn()}
      />,
    );

    // Initially shows checkpoint explanation.
    expect(
      screen.getByText(/advocate revisions are proposed for approval/),
    ).toBeInTheDocument();
  });

  // ── Status panel does not show dry rounds when zero ──────────────────────

  it("hides dry rounds count when zero", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 2,
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
    expect(screen.queryByText("Dry rounds")).not.toBeInTheDocument();
  });

  // ── canStart gating ──────────────────────────────────────────────────────

  it("does not render kickoff when canStart is false and status is active", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: null as unknown as number,
      total_entries: 0,
      update_authority: "checkpoint",
      stop_reason: null,
    };
    const { container } = render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Should show the status panel, not the kickoff button.
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.queryByText("Start refinement")).not.toBeInTheDocument();
  });

  // ── Tribunal debate-trail visibility (integration with DebateTrail) ──────

  it("status panel and debate trail are independent components", () => {
    // Verify that the refinement status panel renders independently of
    // the debate trail — they are separate UI surfaces that share data
    // from the same proposal_show response.
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 3,
      dry_rounds: 1,
      total_entries: 7,
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
    // The refinement panel shows its own data (round, entries, dry rounds).
    expect(screen.getByText("Round")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
    expect(screen.getByText("Entries")).toBeInTheDocument();
    expect(screen.getByText("7")).toBeInTheDocument();
    // The refinement panel does NOT render debate trail entries itself.
    // Debate trail is rendered by the separate <DebateTrail> component.
    expect(screen.queryByText("Debate trail")).not.toBeInTheDocument();
    expect(screen.queryByText("objection")).not.toBeInTheDocument();
    expect(screen.queryByText("verdict")).not.toBeInTheDocument();
  });

  // ── Diff button in checkpoint revisions ────────────────────────────────

  it("renders Diff button for pending checkpoint revisions when loaded", async () => {
    const pendingRev = {
      seq: 3,
      role: "advocate",
      round: 2,
      author_model: "gpt-4o",
      body_preview: "Updated body text",
      title: "Advocate R2 revision",
      created_at: "2026-06-22T10:00:00Z",
    };
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      pending: [pendingRev],
    } as never);

    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      update_authority: "checkpoint",
      stop_reason: null,
      pending_checkpoint_count: 1,
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );

    // Wait for the pending revisions to load.
    expect(await screen.findByText("Advocate R2 revision")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Diff/i })).toBeInTheDocument();
  });

  // ── P4 regression: refinement status panel displays active round info ──

  it("renders active refinement with multiple rounds and entries", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 0,
      total_entries: 12,
      update_authority: "checkpoint",
      stop_reason: null,
      pending_checkpoint_count: 3,
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
    expect(screen.getByText("Round")).toBeInTheDocument();
    expect(screen.getByText("4")).toBeInTheDocument();
    expect(screen.getByText("Entries")).toBeInTheDocument();
    expect(screen.getByText("12")).toBeInTheDocument();
    expect(
      screen.getByText(/3 advocate revision\(s\) awaiting approval/),
    ).toBeInTheDocument();
  });

  it("renders demand round explanation in stopped status", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 3,
      dry_rounds: 1,
      total_entries: 8,
      update_authority: "checkpoint",
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
    expect(screen.getByText(/Adversary exhausted/)).toBeInTheDocument();
  });
});
