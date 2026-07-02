import { describe, expect, it, vi } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import type {
  ProposalDebateTrailRow,
  ProposalRefinementStatus,
} from "@/api/types";
import type { ProposalHistoryEntry } from "@/lib/proposalQueries";
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

  it("renders active in-progress status panel with ribbon and metrics", () => {
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
    expect(screen.getByText("Tribunal")).toBeInTheDocument();
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.getByText("Tribunal running — round 3")).toBeInTheDocument();
    expect(screen.getByText("Refinement in progress")).toBeInTheDocument();
    expect(screen.getAllByText("Entries").length).toBeGreaterThan(0);
    expect(screen.getByText("7")).toBeInTheDocument();
    expect(screen.getByText("Dry rounds")).toBeInTheDocument();
    expect(
      screen.getByText(/running autonomously; you'll review the result/),
    ).toBeInTheDocument();
  });

  it("renders a Blocked gate chip alongside the ribbon", () => {
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
        gateStatus={{
          ready: false,
          dor_ready: true,
          dor_failures: [],
          judge_verdict_body: null,
          judge_verdict_id: null,
          judge_needs_work: false,
          adversary_dry_count: 0,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["e-1"],
          needs_evidence: null,
          human_override_active: false,
          blocked_explanations: [],
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Blocked")).toBeInTheDocument();
  });

  it("renders stopped status panel with stop reason ribbon", () => {
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
    expect(screen.getByText(/falls back to the same model/)).toBeInTheDocument();
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
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: false,
          current_round: 5,
          dry_rounds: 0,
          total_entries: 15,
          stop_reason: "round_cap",
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Stopped")).toBeInTheDocument();
    expect(screen.getByText(/Round cap reached/)).toBeInTheDocument();
  });

  it("renders spawn_cap stop reason label", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: false,
          current_round: 3,
          dry_rounds: 0,
          total_entries: 8,
          stop_reason: "spawn_cap",
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Stopped")).toBeInTheDocument();
    expect(screen.getByText(/Agent spawn cap reached/)).toBeInTheDocument();
  });

  it("renders repeated_objection stop reason label", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: false,
          current_round: 4,
          dry_rounds: 0,
          total_entries: 10,
          stop_reason: "repeated_objection",
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Stopped")).toBeInTheDocument();
    expect(screen.getByText(/Repeated objection detected/)).toBeInTheDocument();
  });

  it("renders agent_failure stop reason label", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: false,
          current_round: 2,
          dry_rounds: 0,
          total_entries: 3,
          stop_reason: "agent_failure",
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Stopped")).toBeInTheDocument();
    expect(screen.getByText(/Agent failure/)).toBeInTheDocument();
  });

  it("renders unknown stop reason as-is", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: false,
          current_round: 1,
          dry_rounds: 0,
          total_entries: 1,
          stop_reason: "some_future_reason",
        }}
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

    expect(showToast.error).toHaveBeenCalledWith("Failed to start refinement", {
      description: "proposal not found",
    });
    expect(onChanged).not.toHaveBeenCalled();
  });

  // ── Autonomous review flow (awaiting_review) ─────────────────────────────

  function reviewStatus(
    overrides: Partial<ProposalRefinementStatus> = {},
  ): ProposalRefinementStatus {
    return {
      active: true,
      current_round: 4,
      dry_rounds: 2,
      total_entries: 12,
      stop_reason: null,
      awaiting_review: true,
      judge_summary: "The spec was tightened and ambiguities resolved.",
      ...overrides,
    };
  }

  function specRevision(seq: number, body: string): ProposalHistoryEntry {
    return {
      id: `rev-${seq}`,
      seq,
      title: "Spec",
      body,
      body_format: "markdown",
      acceptance_criteria: [],
      event_kind: "spec_revision",
      created_at: "2026-06-01T00:00:00Z",
      edited_by_user_id: "user-1",
    } as ProposalHistoryEntry;
  }

  function verdictRow(): ProposalDebateTrailRow {
    return {
      id: "v-1",
      proposal_id: proposalId,
      kind: "verdict",
      body: "Verdict: approve — the spec is ready.",
      blocking: false,
      agent_role: "judge",
      author_kind: "agent",
      author_user_id: null,
      author_model: "gpt",
      source_task_id: null,
      against_revision_seq: 4,
      round: 4,
      resolved_at: null,
      resolved_by_user_id: null,
      reopened_at: null,
      reopened_by_user_id: null,
      created_at: "2026-06-01T00:00:00Z",
      updated_at: "2026-06-01T00:00:00Z",
    };
  }

  it("renders the converged review card with a ribbon, tabs and three actions", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Awaiting review")).toBeInTheDocument();
    expect(
      screen.getByText("Converged after 4 rounds — awaiting your review"),
    ).toBeInTheDocument();
    expect(screen.getByText("Review the result")).toBeInTheDocument();

    // Tabs.
    expect(screen.getByRole("tab", { name: "Verdict" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: /Spec diff/ })).toBeInTheDocument();
    expect(
      screen.getByRole("tab", { name: "Debate trail" }),
    ).toBeInTheDocument();

    // Verdict tab (default) shows the judge summary.
    expect(
      screen.getByText("The spec was tightened and ambiguities resolved."),
    ).toBeInTheDocument();

    // Three unambiguous human choices with distinct accessible names.
    expect(
      screen.getByRole("button", { name: "Accept refined spec" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /Send feedback for another round/ }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Reject and revert" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/Accept keeps the refined spec/)).toBeInTheDocument();
    expect(
      screen.getByText(/Reject reverts to your original spec/),
    ).toBeInTheDocument();
    // The feedback-to-another-round action is disabled until feedback exists.
    expect(
      screen.getByRole("button", { name: /Send feedback for another round/ }),
    ).toBeDisabled();
  });

  it("renders judge_summary markdown semantically in the verdict tab", () => {
    const { container } = render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus({
          judge_summary:
            "**Approved** changes:\n\n- Tightened scope\n- Resolved ambiguity",
        })}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );

    expect(screen.getByText("Approved").tagName).toBe("STRONG");
    expect(screen.getByText("Tightened scope").tagName).toBe("LI");
    expect(screen.getByText("Resolved ambiguity").tagName).toBe("LI");
    expect(container.querySelector(".prose")).toBeInTheDocument();
  });

  it("prefers the gate judge verdict body over the summary in the verdict tab", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus({ judge_summary: "fallback summary" })}
        gateStatus={{
          ready: true,
          dor_ready: true,
          dor_failures: [],
          judge_verdict_body: "Authoritative judge reasoning from the gate.",
          judge_verdict_id: "v-1",
          judge_needs_work: false,
          adversary_dry_count: 0,
          unresolved_blocking_count: 0,
          unresolved_blocking_ids: [],
          needs_evidence: null,
          human_override_active: false,
          blocked_explanations: [],
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText("Authoritative judge reasoning from the gate."),
    ).toBeInTheDocument();
    expect(screen.queryByText("fallback summary")).not.toBeInTheDocument();
  });

  it("falls back to a default summary when judge_summary is blank", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus({ judge_summary: " \n\t " })}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("The tribunal converged.")).toBeInTheDocument();
  });

  it("shows the spec diff and debate trail on their tabs", async () => {
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus({ snapshot_revision_seq: 1 })}
        revisions={[
          specRevision(1, "Original spec."),
          specRevision(2, "Original spec.\nAdded a concrete rollout plan."),
        ]}
        debateTrail={[verdictRow()]}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("tab", { name: /Spec diff/ }));
    expect(
      screen.getByText("Added a concrete rollout plan."),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("tab", { name: "Debate trail" }));
    expect(screen.getByText("Round 4")).toBeInTheDocument();
  });

  it("calls proposal_refinement_resolve with accept on Accept click", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ ok: true } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Accept refined spec" }));

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
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Reject and revert" }));

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

  // ── Send feedback for another round (awaiting_review) ────────────────────

  it("enables the another-round action when feedback is non-blank", async () => {
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    const textarea = screen.getByLabelText(/Feedback/);
    await user.type(textarea, "Please tighten the acceptance criteria.");
    expect(
      screen.getByRole("button", { name: /Send feedback for another round/ }),
    ).toBeEnabled();
  });

  it("calls proposal_refinement_demand_round with feedback as reason", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      proposal_id: proposalId,
      accepted: true,
    } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.type(
      screen.getByLabelText(/Feedback/),
      "Please tighten the acceptance criteria.",
    );
    await user.click(
      screen.getByRole("button", { name: /Send feedback for another round/ }),
    );

    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_refinement_demand_round",
      {
        proposal_id: proposalId,
        reason: "Please tighten the acceptance criteria.",
      },
    );
    expect(showToast.success).toHaveBeenCalledWith(
      "Feedback sent — another tribunal round started",
    );
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("does not call onChanged when demand round fails", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      proposal_id: proposalId,
      accepted: false,
      error: "coordinator not available",
    } as never);

    const onChanged = vi.fn();
    const user = userEvent.setup();
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus()}
        canStart={false}
        onChanged={onChanged}
      />,
    );

    await user.type(screen.getByLabelText(/Feedback/), "Another round please.");
    await user.click(
      screen.getByRole("button", { name: /Send feedback for another round/ }),
    );

    expect(showToast.error).toHaveBeenCalledWith(
      "Failed to send feedback for another round",
      { description: "coordinator not available" },
    );
    expect(onChanged).not.toHaveBeenCalled();
  });

  it("does not render the review card while in progress", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={reviewStatus({
          current_round: 2,
          dry_rounds: 0,
          total_entries: 4,
          awaiting_review: false,
        })}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.queryByText("Review the result")).not.toBeInTheDocument();
    expect(screen.getByText("Refinement in progress")).toBeInTheDocument();
  });

  // ── Status panel does not show dry rounds when zero ──────────────────────

  it("hides dry rounds count when zero", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: true,
          current_round: 1,
          dry_rounds: 0,
          total_entries: 2,
          stop_reason: null,
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.queryByText("Dry rounds")).not.toBeInTheDocument();
  });

  // ── canStart gating ──────────────────────────────────────────────────────

  it("does not render kickoff when canStart is false and status is active", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: true,
          current_round: 1,
          dry_rounds: null as unknown as number,
          total_entries: 0,
          stop_reason: null,
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.queryByText("Start refinement")).not.toBeInTheDocument();
  });

  it("does not render debate-trail entries in the in-progress panel", () => {
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={{
          active: true,
          current_round: 3,
          dry_rounds: 1,
          total_entries: 7,
          stop_reason: null,
        }}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getAllByText("Entries").length).toBeGreaterThan(0);
    // The debate trail is only surfaced in the converged review card's tab.
    expect(screen.queryByText("Debate trail")).not.toBeInTheDocument();
  });
});
