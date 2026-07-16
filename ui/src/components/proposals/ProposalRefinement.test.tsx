import { beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import { fetchUsers, type OrgUser } from "@/api/users";
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

vi.mock("@/api/users", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/users")>();
  return { ...actual, fetchUsers: vi.fn() };
});

vi.mock("@/components/AuthGate", () => ({
  useAuthUser: () => ({ id: "current" }),
}));

vi.mock("@/lib/toast", () => ({
  showToast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

describe("ProposalRefinement", () => {
  const proposalId = "test-proposal-id";
  const participantUsers: OrgUser[] = [
    { id: "author", github_login: "author", github_name: "Author Person", github_avatar_url: null, is_member_of_org: true, is_admin: false },
    { id: "current", github_login: "current", github_name: "Current Person", github_avatar_url: null, is_member_of_org: true, is_admin: false },
    { id: "signer", github_login: "signer", github_name: "Signer Person", github_avatar_url: null, is_member_of_org: true, is_admin: false },
    { id: "outsider", github_login: "outsider", github_name: "Outsider Person", github_avatar_url: null, is_member_of_org: true, is_admin: true },
  ];

  beforeEach(() => {
    vi.clearAllMocks();
    vi.mocked(fetchUsers).mockResolvedValue(participantUsers);
  });

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

  it("limits the owner selector to author and sign-off participants", async () => {
    const user = userEvent.setup();
    render(<ProposalRefinement proposalId={proposalId} status={null} authorUserId="author" signoffUserIds={["current", "signer"]} canStart={true} onChanged={vi.fn()} />);

    await screen.findByText("Current Person");
    await user.click(screen.getByRole("combobox"));

    expect(screen.getByRole("option", { name: "Author Person" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Current Person" })).toBeInTheDocument();
    expect(screen.getByRole("option", { name: "Signer Person" })).toBeInTheDocument();
    expect(screen.queryByRole("option", { name: "Outsider Person" })).not.toBeInTheDocument();
  });

  it("defaults refinement ownership to the current participant", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ refinement: { active: true } } as never);
    const user = userEvent.setup();
    render(<ProposalRefinement proposalId={proposalId} status={null} authorUserId="author" signoffUserIds={["current"]} canStart={true} onChanged={vi.fn()} />);

    await screen.findByText("Current Person");
    await user.click(screen.getByRole("button", { name: "Start refinement" }));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_start", {
      proposal_id: proposalId,
      owner_user_id: "current",
    });
  });

  it("uses the first participant when the current user is not a participant", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ refinement: { active: true } } as never);
    const user = userEvent.setup();
    render(<ProposalRefinement proposalId={proposalId} status={null} authorUserId="author" signoffUserIds={["signer"]} canStart={true} onChanged={vi.fn()} />);

    await screen.findByText("Author Person");
    await user.click(screen.getByRole("button", { name: "Start refinement" }));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_start", {
      proposal_id: proposalId,
      owner_user_id: "author",
    });
  });

  it("sends the participant selected in the owner selector", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ refinement: { active: true } } as never);
    const user = userEvent.setup();
    render(<ProposalRefinement proposalId={proposalId} status={null} authorUserId="author" signoffUserIds={["current", "signer"]} canStart={true} onChanged={vi.fn()} />);

    await screen.findByText("Current Person");
    await user.click(screen.getByRole("combobox"));
    await user.click(screen.getByRole("option", { name: "Signer Person" }));
    await user.click(screen.getByRole("button", { name: "Start refinement" }));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_start", {
      proposal_id: proposalId,
      owner_user_id: "signer",
    });
  });

  it("renders the durable owner returned by refreshed refinement status", async () => {
    render(<ProposalRefinement proposalId={proposalId} status={{ active: true, owner_user_id: "signer", current_round: 1, dry_rounds: 0, total_entries: 1, stop_reason: null }} canStart={false} onChanged={vi.fn()} />);

    expect(await screen.findByText("Signer Person")).toBeInTheDocument();
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
        authorUserId="author"
        canStart={true}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByText("Start refinement"));

    expect(callMcpTool).toHaveBeenCalledWith("proposal_refinement_start", {
      proposal_id: proposalId,
      owner_user_id: "author",
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
        authorUserId="author"
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

  // ── Evidence lifecycle states ──────────────────────────────────────────────

  it("renders AwaitingEvidence with spike id, status, and claim", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "awaiting_evidence",
      needs_evidence: {
        claim: "X is load-bearing for the subsystem",
        spike_task_id: "uuid-spike",
        spike_short_id: "ab12",
        spike_status: "open",
        evidence_phase: "awaiting_evidence",
        question: "Does X handle failover correctly?",
        target_subsystem: "coordinator",
        spec_unknown_anchor: "failover behavior",
        round: 2,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Badge
    expect(screen.getAllByText("Awaiting evidence").length).toBeGreaterThanOrEqual(2);
    // Ribbon includes spike id and status
    expect(
      screen.getByText(/Awaiting evidence — spike ab12 \(open\)/),
    ).toBeInTheDocument();
    // Body section
    expect(
      screen.getByText(/The tribunal has parked refinement pending evidence/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Claim: X is load-bearing for the subsystem/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Question: Does X handle failover correctly?/),
    ).toBeInTheDocument();
    expect(screen.getByText(/Target: coordinator/)).toBeInTheDocument();
    // Does NOT show stale in-progress copy
    expect(
      screen.queryByText("Refinement in progress"),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Active")).not.toBeInTheDocument();
  });

  it("renders EvidenceFailed with failure reason and remediation copy", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 3,
      dry_rounds: 0,
      total_entries: 6,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "evidence_failed",
      needs_evidence: {
        claim: "Y handles concurrent access safely",
        spike_task_id: "uuid-spike-2",
        spike_short_id: "cd34",
        spike_status: "closed",
        evidence_phase: "evidence_failed",
        failure_reason: "spike_force_closed",
        round: 3,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Badge + body label both say "Evidence failed"
    expect(screen.getAllByText("Evidence failed").length).toBeGreaterThanOrEqual(2);
    // Ribbon includes spike id and failure reason
    expect(
      screen.getByText(
        /Evidence failed — spike cd34 \(spike_force_closed\)/,
      ),
    ).toBeInTheDocument();
    // Body section
    expect(screen.getByText(/was force-closed/)).toBeInTheDocument();
    expect(
      screen.getByText(/Review the spike activity, address the underlying issue/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Claim: Y handles concurrent access safely/),
    ).toBeInTheDocument();
    // Does NOT show stale states
    expect(
      screen.queryByText("Refinement in progress"),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Active")).not.toBeInTheDocument();
    expect(screen.queryByText("Awaiting review")).not.toBeInTheDocument();
    expect(screen.queryByText("Converged")).not.toBeInTheDocument();
  });

  it("renders PausedOrFrozen with manual gate copy and evidence context", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "paused_or_frozen",
      needs_evidence: {
        claim: "Z is safe to deploy",
        spike_task_id: "uuid-spike-3",
        spike_short_id: "ef56",
        spike_status: "closed",
        evidence_phase: "evidence_received",
        round: 2,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Badge
    expect(screen.getByText("Paused")).toBeInTheDocument();
    // Ribbon + body label both say "Paused — waiting on manual gate"
    expect(
      screen.getAllByText(/Paused — waiting on manual gate/).length,
    ).toBeGreaterThanOrEqual(2);
    // Manual gate copy with evidence context
    expect(
      screen.getByText(/Refinement is paused until a manual gate is cleared/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Evidence has been collected and is ready for review/),
    ).toBeInTheDocument();
    expect(screen.getAllByText(/ef56/).length).toBeGreaterThan(0);
    // Does NOT show stale in-progress copy
    expect(
      screen.queryByText("Refinement in progress"),
    ).not.toBeInTheDocument();
  });

  it("renders PausedOrFrozen without evidence context when no spike", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 2,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "paused_or_frozen",
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Paused")).toBeInTheDocument();
    // Ribbon + body label both say "Paused — waiting on manual gate"
    expect(
      screen.getAllByText(/Paused — waiting on manual gate/).length,
    ).toBeGreaterThanOrEqual(2);
    expect(
      screen.getByText(/Refinement is paused until a manual gate is cleared/),
    ).toBeInTheDocument();
    // No evidence context when no spike
    expect(screen.queryByText(/Evidence has been collected/)).not.toBeInTheDocument();
  });

  it("renders EvidenceReceived with spike details", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 3,
      dry_rounds: 0,
      total_entries: 6,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "evidence_received",
      needs_evidence: {
        claim: "Performance is acceptable",
        spike_task_id: "uuid-spike-4",
        spike_short_id: "gh78",
        spike_status: "closed",
        evidence_phase: "evidence_received",
        round: 3,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Badge + body label both say "Evidence received"
    expect(screen.getAllByText("Evidence received").length).toBeGreaterThanOrEqual(2);
    expect(
      screen.getByText(/Evidence received — waiting on refinement to resume/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Evidence has been collected and the spike findings are available/),
    ).toBeInTheDocument();
    expect(screen.getByText(/gh78/)).toBeInTheDocument();
    // Does NOT show in-progress
    expect(
      screen.queryByText("Refinement in progress"),
    ).not.toBeInTheDocument();
  });

  it("does not render evidence sections for normal active state", () => {
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
    expect(screen.getByText("Active")).toBeInTheDocument();
    expect(screen.getByText("Refinement in progress")).toBeInTheDocument();
    expect(screen.queryByText("Awaiting evidence")).not.toBeInTheDocument();
    expect(screen.queryByText("Evidence failed")).not.toBeInTheDocument();
    expect(screen.queryByText("Evidence received")).not.toBeInTheDocument();
    expect(screen.queryByText("Paused")).not.toBeInTheDocument();
  });

  it("does not render evidence sections for converged awaiting_review state", () => {
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
      screen.getByText(/Converged after 4 rounds/),
    ).toBeInTheDocument();
    expect(screen.queryByText("Awaiting evidence")).not.toBeInTheDocument();
    expect(screen.queryByText("Evidence failed")).not.toBeInTheDocument();
  });

  it("paused_or_frozen with evidence takes precedence and suppresses auto-resume copy", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 2,
      dry_rounds: 0,
      total_entries: 4,
      stop_reason: null,
      awaiting_review: false,
      evidence_lifecycle_state: "paused_or_frozen",
      needs_evidence: {
        claim: "Subsystem handles backpressure",
        spike_task_id: "uuid-spike-pause",
        spike_short_id: "pa99",
        spike_status: "closed",
        evidence_phase: "evidence_received",
        round: 2,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Badge shows Paused, not "Evidence received" or "Awaiting evidence"
    expect(screen.getByText("Paused")).toBeInTheDocument();
    // Manual gate copy
    expect(
      screen.getByText(/Refinement is paused until a manual gate is cleared/),
    ).toBeInTheDocument();
    // Evidence context visible
    expect(
      screen.getByText(/Evidence has been collected and is ready for review/),
    ).toBeInTheDocument();
    expect(screen.getByText(/pa99/)).toBeInTheDocument();
    // Suppressed states — no auto-resume or running copy
    expect(
      screen.queryByText("Refinement in progress"),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText(/running autonomously; you'll review/),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Active")).not.toBeInTheDocument();
    expect(screen.queryByText("Evidence received")).not.toBeInTheDocument();
    expect(screen.queryByText("Awaiting evidence")).not.toBeInTheDocument();
  });

  it("renders evidence_failed via needs_evidence fallback when lifecycle state is absent", () => {
    const status: ProposalRefinementStatus = {
      active: true,
      current_round: 1,
      dry_rounds: 0,
      total_entries: 3,
      stop_reason: null,
      awaiting_review: false,
      needs_evidence: {
        claim: "Database migration is safe",
        spike_task_id: "uuid-spike-fallback",
        spike_short_id: "fb12",
        spike_status: "error",
        evidence_phase: "evidence_failed",
        failure_reason: "spike_errored",
        round: 1,
      },
    };
    render(
      <ProposalRefinement
        proposalId={proposalId}
        status={status}
        canStart={false}
        onChanged={vi.fn()}
      />,
    );
    // Detected as evidence_failed via needs_evidence fields
    expect(screen.getAllByText("Evidence failed").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText(/encountered an error/)).toBeInTheDocument();
    expect(screen.getAllByText(/fb12/).length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText(/Claim: Database migration is safe/)).toBeInTheDocument();
    // No stale states
    expect(screen.queryByText("Refinement in progress")).not.toBeInTheDocument();
    expect(screen.queryByText("Active")).not.toBeInTheDocument();
  });
});
