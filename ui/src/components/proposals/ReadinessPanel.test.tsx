import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import type {
  ProposalGateStatus,
  ProposalRefinementStatus,
  CheckpointRevision,
} from "@/api/types";
import { ReadinessPanel } from "./ReadinessPanel";
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

function gateStatus(
  overrides: Partial<ProposalGateStatus> = {},
): ProposalGateStatus {
  return {
    ready: true,
    dor_ready: true,
    dor_failures: [],
    judge_verdict_body: null,
    judge_verdict_id: null,
    judge_needs_work: false,
    adversary_dry_count: 0,
    unresolved_blocking_count: 0,
    unresolved_blocking_ids: [],
    needs_evidence: null,
    pending_checkpoint: false,
    human_override_active: false,
    blocked_explanations: [],
    ...overrides,
  };
}

function refinement(
  overrides: Partial<ProposalRefinementStatus> = {},
): ProposalRefinementStatus {
  return {
    active: true,
    current_round: 1,
    dry_rounds: 0,
    total_entries: 0,
    update_authority: "checkpoint",
    stop_reason: null,
    pending_checkpoint_count: 0,
    ...overrides,
  };
}

function checkpointRevision(
  overrides: Partial<CheckpointRevision> = {},
): CheckpointRevision {
  return {
    seq: 1,
    role: "advocate",
    round: 1,
    author_model: "gpt-4o",
    body_preview: "Updated proposal body preview",
    title: "Advocate revision",
    created_at: "2026-06-20T10:00:00Z",
    ...overrides,
  };
}

describe("ReadinessPanel", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
    vi.mocked(showToast.success).mockClear();
    vi.mocked(showToast.error).mockClear();
  });

  // ── Rendering basics ────────────────────────────────────────────────────

  it("renders nothing when gateStatus and refinement are both null", () => {
    const { container } = render(
      <ReadinessPanel
        gateStatus={null}
        refinement={null}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(container.innerHTML).toBe("");
  });

  it("renders Ready badge when gate passes", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Ready")).toBeInTheDocument();
  });

  it("renders Blocked badge when gate is blocked", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          blocked_explanations: ["Missing problem section"],
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Blocked")).toBeInTheDocument();
  });

  // ── DoR status ──────────────────────────────────────────────────────────

  it("renders DoR pass when dor_ready is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ dor_ready: true })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/Definition of Ready: Pass/)).toBeInTheDocument();
  });

  it("renders DoR failures with check names when dor_ready is false", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          dor_ready: false,
          dor_failures: [
            { check: "problem_coverage", message: "Missing required coverage: problem" },
            { check: "grounding", message: "Missing grounding: add entry points" },
          ],
          blocked_explanations: [
            "Missing required coverage: problem",
            "Missing grounding: add entry points",
          ],
          ready: false,
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/Definition of Ready: Fail/)).toBeInTheDocument();
    expect(screen.getByText(/Problem coverage/)).toBeInTheDocument();
    expect(screen.getByText(/Missing required coverage: problem/)).toBeInTheDocument();
    expect(screen.getByText(/Grounding/)).toBeInTheDocument();
  });

  // ── Judge verdict ───────────────────────────────────────────────────────

  it("renders judge verdict badge as Ready when judge is not needs-work", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Looks good to me",
          judge_verdict_id: "v-1",
          judge_needs_work: false,
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    expect(screen.getByText("Ready")).toBeInTheDocument();
  });

  it("renders judge verdict badge as Needs-work when judge_needs_work is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Needs-work: missing scope section",
          judge_verdict_id: "v-2",
          judge_needs_work: true,
          ready: false,
          blocked_explanations: [
            "Judge returned needs-work (verdict v-2); no current human override",
          ],
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    expect(screen.getByText("Needs-work")).toBeInTheDocument();
  });

  // ── Adversary dry count ─────────────────────────────────────────────────

  it("renders adversary dry count when > 0", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ adversary_dry_count: 2 })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/Adversary dry:/)).toBeInTheDocument();
    expect(screen.getByText("2")).toBeInTheDocument();
  });

  // ── Unresolved blocking rows ────────────────────────────────────────────

  it("renders unresolved blocking count when > 0", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          unresolved_blocking_count: 3,
          unresolved_blocking_ids: ["e-1", "e-2", "e-3"],
          ready: false,
          blocked_explanations: ["Unresolved blocking debate entries: e-1, e-2, e-3"],
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/Blocking rows:/)).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  // ── Needs-evidence spike ────────────────────────────────────────────────

  it("renders needs-evidence spike parking state", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          needs_evidence: {
            claim: "X is load-bearing",
            spike_task_id: "uuid-spike",
            spike_short_id: "ab12",
            spike_status: "open",
          },
          ready: false,
          blocked_explanations: [
            "Proposal parked on needs-evidence spike ab12 (claim: X is load-bearing)",
          ],
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Parked: needs-evidence spike")).toBeInTheDocument();
    expect(screen.getByText(/X is load-bearing/)).toBeInTheDocument();
    expect(screen.getByText(/ab12/)).toBeInTheDocument();
    expect(screen.getByText(/open/)).toBeInTheDocument();
  });

  // ── Blocked explanations ────────────────────────────────────────────────

  it("renders blocked explanations naming exact failures", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          blocked_explanations: [
            "Missing required coverage: problem",
            "Pending checkpoint revision awaiting approval or rejection",
            "Judge returned needs-work (verdict v-1); no current human override",
          ],
        })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Blocked because")).toBeInTheDocument();
    expect(screen.getByText(/Missing required coverage: problem/)).toBeInTheDocument();
    expect(screen.getByText(/Pending checkpoint revision/)).toBeInTheDocument();
    expect(screen.getByText(/Judge returned needs-work/)).toBeInTheDocument();
  });

  // ── Human override badge ────────────────────────────────────────────────

  it("renders Human override badge when human_override_active is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ human_override_active: true })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText("Human override")).toBeInTheDocument();
  });

  it("does not render Human override badge when human_override_active is false", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ human_override_active: false })}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.queryByText("Human override")).not.toBeInTheDocument();
  });

  // ── Checkpoint mode explanation ─────────────────────────────────────────

  it("renders checkpoint mode explanation when refinement is active and checkpoint", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement({ update_authority: "checkpoint" })}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/Checkpoint mode: advocate revisions require explicit approval/),
    ).toBeInTheDocument();
  });

  it("renders auto-accept mode explanation when refinement is active and auto_accept", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement({ update_authority: "auto_accept" })}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/Auto-accept mode: advocate revisions are applied automatically/),
    ).toBeInTheDocument();
  });

  // ── Checkpoint revisions with diff ──────────────────────────────────────

  it("renders pending checkpoint revisions with Diff and Approve/Reject buttons", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision()]}
        onChanged={vi.fn()}
      />,
    );
    expect(screen.getByText(/Pending revisions/)).toBeInTheDocument();
    expect(screen.getByText("Advocate revision")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Diff/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Approve/i })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Reject/i })).toBeInTheDocument();
  });

  it("shows diff preview when Diff button is clicked", async () => {
    const user = userEvent.setup();
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision()]}
        onChanged={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Diff/i }));

    expect(
      screen.getByText(/Preview of proposed revision/),
    ).toBeInTheDocument();
    // Button text should change to "Hide diff"
    expect(screen.getByRole("button", { name: /Hide diff/i })).toBeInTheDocument();
  });

  // ── Checkpoint approve/reject actions ───────────────────────────────────

  it("calls proposal_refinement_checkpoint_approve on Approve click", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ approved: true } as never);
    const onChanged = vi.fn();
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision({ seq: 42 })]}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Approve/i }));

    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_refinement_checkpoint_approve",
      { proposal_id: "p-1", revision_seq: 42 },
    );
    expect(showToast.success).toHaveBeenCalledWith("Checkpoint revision approved");
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("calls proposal_refinement_checkpoint_reject on Reject click", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({ rejected: true } as never);
    const onChanged = vi.fn();
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision({ seq: 7 })]}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Reject/i }));

    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_refinement_checkpoint_reject",
      { proposal_id: "p-1", revision_seq: 7 },
    );
    expect(showToast.success).toHaveBeenCalledWith("Checkpoint revision rejected");
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("shows error toast when checkpoint approve fails", async () => {
    vi.mocked(callMcpTool).mockResolvedValueOnce({
      error: "not found",
    } as never);
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision()]}
        onChanged={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Approve/i }));

    expect(showToast.error).toHaveBeenCalledWith(
      "Failed to approve revision",
      { description: "not found" },
    );
  });

  it("shows error toast when checkpoint reject throws", async () => {
    vi.mocked(callMcpTool).mockRejectedValueOnce(new Error("Network error"));
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement()}
        proposalId="p-1"
        pendingRevisions={[checkpointRevision()]}
        onChanged={vi.fn()}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Reject/i }));

    expect(showToast.error).toHaveBeenCalledWith(
      "Failed to reject revision",
      { description: "Network error" },
    );
  });

  // ── Renders with refinement-only data (no gate_status) ──────────────────

  it("renders mode explanation when gateStatus is null but refinement exists", () => {
    render(
      <ReadinessPanel
        gateStatus={null}
        refinement={refinement({ update_authority: "auto_accept" })}
        proposalId="p-1"
        pendingRevisions={[]}
        onChanged={vi.fn()}
      />,
    );
    expect(
      screen.getByText(/Auto-accept mode/),
    ).toBeInTheDocument();
  });
});
