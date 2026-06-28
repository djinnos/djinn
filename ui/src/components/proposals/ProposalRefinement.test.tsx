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
  });

  it("renders active in-progress status panel", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 3,
      dry_rounds: 1,
      total_entries: 7,
      stop_reason: null,
      awaiting_review: false,
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
    expect(screen.getByText("Refinement in progress")).toBeInTheDocument();
    expect(screen.getAllByText("Entries").length).toBeGreaterThan(0);
    expect(screen.getByText("7")).toBeInTheDocument();
    expect(screen.getByText("Dry rounds")).toBeInTheDocument();
    expect(
      screen.getByText(/running autonomously; you'll review the result/),
    ).toBeInTheDocument();
  });

  it("renders stopped status panel with stop reason", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 5,
      dry_rounds: 2,
      total_entries: 12,
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

  it("offers a restart button for an interrupted refinement", () => {
    // A stopped refinement must offer a way forward — otherwise an
    // interrupted run (e.g. one lost across a deploy) is a dead end in the UI.
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 0,
      stop_reason: "interrupted",
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
      screen.getByRole("button", { name: /Restart refinement/ }),
    ).toBeInTheDocument();
  });

  it("explains same-model fallback in the status panel", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 0,
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

  it("explains the autonomous review flow in the kickoff affordance", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={null}
        canStart={true}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/when it converges you'll review the full refined result/),
    ).toBeInTheDocument();
  });

  // ── All stop reasons ─────────────────────────────────────────────────────

  it("renders round_cap stop reason label", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 5,
      dry_rounds: 0,
      total_entries: 15,
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
      refinement: { active: true },
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

  // ── Autonomous review flow (awaiting_review) ─────────────────────────────

  it("renders the review panel with Accept/Reject when awaiting_review", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary: "The spec was tightened and ambiguities resolved.",
      snapshot_revision_seq: 3,
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
      screen.getByText("Tribunal converged — review the result"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("The spec was tightened and ambiguities resolved."),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/the diff from your original is in the revision history/),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Accept" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Reject" }),
    ).toBeInTheDocument();
  });

  it("renders judge_summary markdown semantically", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary:
        "**Approved** changes:\n\n- Tightened scope\n- Resolved ambiguity",
    };
    const { container } = render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );

    expect(screen.getByText("Approved").tagName).toBe("STRONG");
    expect(screen.getByRole("list")).toBeInTheDocument();
    expect(screen.getByText("Tightened scope").tagName).toBe("LI");
    expect(screen.getByText("Resolved ambiguity").tagName).toBe("LI");
    expect(container.querySelector(".prose")).toBeInTheDocument();
  });

  it("falls back to a default summary when judge_summary is blank", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary: " \n\t ",
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("The tribunal converged.")).toBeInTheDocument();
  });

  it("calls proposal_refinement_resolve with accept on Accept click", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ ok: true } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary: "Converged.",
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Accept" }));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_resolve", {
      proposal_id: proposalId,
      decision: "accept",
      feedback: undefined,
    });
    expect(showToast.success).toHaveBeenCalledWith("Refined spec accepted");
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("calls proposal_refinement_resolve with reject on Reject click", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ ok: true } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary: "Converged.",
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Reject" }));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_resolve", {
      proposal_id: proposalId,
      decision: "reject",
      feedback: undefined,
    });
    expect(showToast.success).toHaveBeenCalledWith(
      "Refinement rejected — reverted to your original",
    );
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("does not render the review panel while in progress", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      stop_reason: null,
      awaiting_review: false,
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
      screen.queryByText("Tribunal converged — review the result"),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Refinement in progress")).toBeInTheDocument();
  });

  // ── Status panel does not show dry rounds when zero ──────────────────────

  it("hides dry rounds count when zero", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 2,
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
    // The refinement panel shows its own data (entries, dry rounds).
    expect(screen.getAllByText("Entries").length).toBeGreaterThan(0);
    expect(screen.getByText("7")).toBeInTheDocument();
    // The refinement panel does NOT render debate trail entries itself.
    // Debate trail is rendered by the separate <DebateTrail> component.
    expect(screen.queryByText("Debate trail")).not.toBeInTheDocument();
    expect(screen.queryByText("objection")).not.toBeInTheDocument();
    expect(screen.queryByText("verdict")).not.toBeInTheDocument();
  });

  it("renders demand round explanation in stopped status", () => {
    const status: ProposalRefinementStatus = {
      active: false,
      current_round: 3,
      dry_rounds: 1,
      total_entries: 8,
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
