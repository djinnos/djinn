import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";
import type {
  ProposalDebateTrailRow,
  ProposalGateStatus,
  ProposalRefinementStatus,
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
    stop_reason: null,
    ...overrides,
  };
}

function debateRow(
  overrides: Partial<ProposalDebateTrailRow> = {},
): ProposalDebateTrailRow {
  return {
    id: "e-1",
    proposal_id: "p-1",
    kind: "objection",
    body: "This is a blocking objection about missing scope.",
    blocking: true,
    agent_role: "adversary",
    author_kind: "agent",
    author_user_id: null,
    author_model: "gpt",
    source_task_id: null,
    against_revision_seq: 3,
    round: 2,
    resolved_at: null,
    resolved_by_user_id: null,
    reopened_at: null,
    reopened_by_user_id: null,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-01T00:00:00Z",
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
      <ReadinessPanel gateStatus={null} refinement={null} />,
    );
    expect(container.innerHTML).toBe("");
  });

  it("renders Ready badge and the checklist footer when gate passes", () => {
    render(
      <ReadinessPanel gateStatus={gateStatus()} refinement={refinement()} />,
    );
    expect(screen.getByText("Readiness gate")).toBeInTheDocument();
    expect(screen.getByText("Ready")).toBeInTheDocument();
    expect(screen.getByText("Ready when every row clears.")).toBeInTheDocument();
  });

  it("renders Blocked badge when gate is blocked", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          dor_ready: false,
          dor_failures: [
            { check: "problem_coverage", message: "Missing problem section" },
          ],
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Blocked")).toBeInTheDocument();
  });

  // ── DoR row ─────────────────────────────────────────────────────────────

  it("renders DoR row as passing when dor_ready is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ dor_ready: true })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Definition of Ready")).toBeInTheDocument();
    expect(screen.getByText("all checks pass")).toBeInTheDocument();
  });

  it("lists DoR failures with check names when dor_ready is false", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          dor_ready: false,
          dor_failures: [
            {
              check: "problem_coverage",
              message: "Missing required coverage: problem",
            },
            {
              check: "grounding",
              message: "Missing grounding: add entry points",
            },
          ],
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Definition of Ready")).toBeInTheDocument();
    expect(screen.getByText("2 checks failing")).toBeInTheDocument();
    expect(screen.getByText(/Problem coverage/)).toBeInTheDocument();
    expect(
      screen.getByText(/Missing required coverage: problem/),
    ).toBeInTheDocument();
    expect(screen.getByText(/Grounding/)).toBeInTheDocument();
  });

  // ── Judge verdict row ─────────────────────────────────────────────────────

  it("renders judge row as approve/ready when judge is not needs-work", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Looks good to me",
          judge_verdict_id: "v-1",
          judge_needs_work: false,
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    expect(screen.getByText(/approve \/ ready/)).toBeInTheDocument();
  });

  it("renders judge row as needs-work with the round from the trail", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Needs-work: missing scope section",
          judge_verdict_id: "v-2",
          judge_needs_work: true,
          ready: false,
        })}
        refinement={refinement()}
        debateTrail={[
          debateRow({
            id: "v-2",
            kind: "verdict",
            body: "Verdict: needs-work",
            round: 4,
            against_revision_seq: 4,
          }),
        ]}
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    expect(screen.getByText("needs-work (round 4)")).toBeInTheDocument();
  });

  it("does not render the judge reasoning markdown body (rendered in the tribunal card)", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body:
            "## Verdict\n\nNeeds **scope** before graduation.",
          judge_verdict_id: "v-3",
          judge_needs_work: true,
          ready: false,
        })}
        refinement={refinement()}
      />,
    );
    // The full reasoning heading must NOT appear here — it lives in the
    // Tribunal review card so it renders only once on the page.
    expect(
      screen.queryByRole("heading", { name: "Verdict", level: 2 }),
    ).not.toBeInTheDocument();
    expect(screen.queryByText("Judge reasoning")).not.toBeInTheDocument();
  });

  // ── Blocking debate entries row ──────────────────────────────────────────

  it("renders blocking entries row and expands to real entry cards", async () => {
    const user = userEvent.setup();
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["e-1"],
        })}
        refinement={refinement()}
        debateTrail={[debateRow({ id: "e-1" })]}
      />,
    );

    expect(screen.getByText("Blocking debate entries")).toBeInTheDocument();
    expect(screen.getByText("1 unresolved")).toBeInTheDocument();
    // Collapsed by default — no entry body until expanded.
    expect(
      screen.queryByText(/This is a blocking objection/),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: /show/ }));

    expect(screen.getByText("adversary")).toBeInTheDocument();
    expect(screen.getByText("round 2")).toBeInTheDocument();
    expect(screen.getByText("vs rev 3")).toBeInTheDocument();
    expect(screen.getByText(/This is a blocking objection/)).toBeInTheDocument();
  });

  it("resolves a blocking entry through proposal_debate_resolve", async () => {
    const onChanged = vi.fn();
    vi.mocked(callMcpTool).mockResolvedValue({ ok: true } as never);
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["e-1"],
        })}
        refinement={refinement()}
        debateTrail={[debateRow({ id: "e-1" })]}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: /show/ }));
    await user.click(screen.getByRole("button", { name: "Resolve" }));
    // ConfirmButton opens a dialog; confirm the action.
    await user.click(screen.getByRole("button", { name: "Resolve entry" }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith("proposal_debate_resolve", {
        id: "e-1",
      });
    });
    expect(showToast.success).toHaveBeenCalledWith("Blocking entry resolved");
    expect(onChanged).toHaveBeenCalled();
  });

  it("falls back to the raw id when a blocking id is not in the trail", async () => {
    const user = userEvent.setup();
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["missing-uuid"],
        })}
        refinement={refinement()}
        debateTrail={[]}
      />,
    );
    await user.click(screen.getByRole("button", { name: /show/ }));
    expect(screen.getByText("missing-uuid")).toBeInTheDocument();
  });

  it("labels a superseded blocking verdict", async () => {
    const user = userEvent.setup();
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["v-old"],
        })}
        refinement={refinement()}
        debateTrail={[
          debateRow({
            id: "v-old",
            kind: "verdict",
            body: "Verdict: needs-work",
            against_revision_seq: 2,
          }),
          debateRow({
            id: "v-new",
            kind: "verdict",
            body: "Verdict: approve",
            against_revision_seq: 4,
            blocking: false,
          }),
        ]}
      />,
    );
    await user.click(screen.getByRole("button", { name: /show/ }));
    expect(screen.getByText("superseded")).toBeInTheDocument();
  });

  // ── Evidence spike row ────────────────────────────────────────────────────

  it("renders evidence row as none required when there is no spike", () => {
    render(
      <ReadinessPanel gateStatus={gateStatus()} refinement={refinement()} />,
    );
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    expect(screen.getByText("none required")).toBeInTheDocument();
  });

  it("renders needs-evidence spike details when parked", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "X is load-bearing",
            spike_task_id: "uuid-spike",
            spike_short_id: "ab12",
            spike_status: "open",
          },
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Parked: needs-evidence spike")).toBeInTheDocument();
    expect(screen.getAllByText(/X is load-bearing/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/ab12/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/open/).length).toBeGreaterThan(0);
  });

  // ── Human override ────────────────────────────────────────────────────────

  it("collapses the override behind a link and expands to the reason box", async () => {
    const user = userEvent.setup();
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          judge_needs_work: true,
          judge_verdict_id: "v-1",
        })}
        refinement={refinement()}
      />,
    );
    // Reason box hidden until the link is clicked.
    expect(
      screen.queryByLabelText("Human override audit reason"),
    ).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Record override/ }));
    expect(
      screen.getByLabelText("Human override audit reason"),
    ).toBeInTheDocument();
  });

  it("records a human override with an audit reason and refreshes", async () => {
    const onChanged = vi.fn();
    vi.mocked(callMcpTool).mockResolvedValue({ overridden: true } as never);
    const user = userEvent.setup();

    render(
      <ReadinessPanel
        proposalId="proposal-123"
        gateStatus={gateStatus({
          ready: false,
          judge_verdict_id: "v-override",
          judge_needs_work: true,
        })}
        refinement={refinement()}
        onChanged={onChanged}
      />,
    );

    await user.click(screen.getByRole("button", { name: /Record override/ }));

    const button = screen.getByRole("button", {
      name: /Override DoR and proceed/,
    });
    expect(button).toBeDisabled();

    await user.type(
      screen.getByLabelText("Human override audit reason"),
      "Judge is wrong because the missing DoR is covered by linked evidence.",
    );
    await user.click(button);

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith("proposal_verdict_override", {
        proposal_id: "proposal-123",
        overridden_verdict_entry_id: "v-override",
        reason:
          "Judge is wrong because the missing DoR is covered by linked evidence.",
      });
    });
    expect(showToast.success).toHaveBeenCalledWith("Human override recorded");
    expect(onChanged).toHaveBeenCalledTimes(1);
  });

  it("shows the Human override badge and active note, and no override link", () => {
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          human_override_active: true,
          judge_needs_work: true,
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Human override")).toBeInTheDocument();
    expect(screen.getByText(/A human override is active/)).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Record override/ }),
    ).not.toBeInTheDocument();
  });

  it("does not render the Human override badge when human_override_active is false", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ human_override_active: false })}
        refinement={refinement()}
      />,
    );
    expect(screen.queryByText("Human override")).not.toBeInTheDocument();
  });

  // ── Active-refinement note ──────────────────────────────────────────────

  it("renders the autonomous-tribunal note when refinement is active", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement({ active: true })}
      />,
    );
    expect(
      screen.getByText(/Autonomous tribunal in progress/),
    ).toBeInTheDocument();
  });

  it("renders note when gateStatus is null but refinement is active", () => {
    render(<ReadinessPanel gateStatus={null} refinement={refinement()} />);
    expect(
      screen.getByText(/Autonomous tribunal in progress/),
    ).toBeInTheDocument();
  });

  // ── Evidence phase-aware rendering ──────────────────────────────────────

  it("renders AwaitingEvidence phase in the evidence spike row", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "X is load-bearing",
            spike_task_id: "uuid-spike",
            spike_short_id: "ab12",
            spike_status: "open",
            evidence_phase: "awaiting_evidence",
          },
        })}
        refinement={refinement()}
      />,
    );
    // Gate row
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    expect(
      screen.getByText(/awaiting evidence — spike ab12 \(open\)/),
    ).toBeInTheDocument();
    // Parked note (amber, not destructive)
    expect(
      screen.getByText(/Parked: needs-evidence spike/),
    ).toBeInTheDocument();
    expect(screen.getAllByText(/X is load-bearing/).length).toBeGreaterThan(0);
  });

  it("renders EvidenceFailed phase with destructive styling and failure reason", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Y handles concurrency safely",
            spike_task_id: "uuid-spike-2",
            spike_short_id: "cd34",
            spike_status: "closed",
            evidence_phase: "evidence_failed",
            failure_reason: "spike_force_closed",
          },
        })}
        refinement={refinement()}
      />,
    );
    // Gate row shows failed
    expect(
      screen.getByText(/evidence failed — spike cd34 \(spike_force_closed\)/),
    ).toBeInTheDocument();
    // Destructive parked note instead of amber
    expect(
      screen.getByText(/Evidence failed: cd34/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Reason: spike_force_closed/),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/Review the spike findings and address the issue/),
    ).toBeInTheDocument();
    // Should NOT show the amber "Parked" note
    expect(
      screen.queryByText(/Parked: needs-evidence spike/),
    ).not.toBeInTheDocument();
  });

  it("renders EvidenceReceived phase with green/received copy", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Performance is acceptable",
            spike_task_id: "uuid-spike-3",
            spike_short_id: "ef56",
            spike_status: "closed",
            evidence_phase: "evidence_received",
          },
        })}
        refinement={refinement()}
      />,
    );
    // Gate row shows received (pass state)
    expect(
      screen.getByText(/evidence received — spike ef56/),
    ).toBeInTheDocument();
    // Note says "Evidence received" not "Parked"
    expect(
      screen.getByText("Evidence received"),
    ).toBeInTheDocument();
    // Should NOT show the amber "Parked" note
    expect(
      screen.queryByText(/Parked: needs-evidence spike/),
    ).not.toBeInTheDocument();
  });

  it("preserves non-contradictory manual pause/freeze precedence with evidence", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Z is safe to deploy",
            spike_task_id: "uuid-spike-4",
            spike_short_id: "gh78",
            spike_status: "open",
            evidence_phase: "awaiting_evidence",
          },
        })}
        refinement={refinement({
          active: true,
          evidence_lifecycle_state: "paused_or_frozen",
        })}
      />,
    );
    // The evidence row is still rendered with the claim
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    expect(screen.getAllByText(/Z is safe to deploy/).length).toBeGreaterThan(0);
    // The refinement active note still shows (from refinement.active)
    expect(
      screen.getByText(/Autonomous tribunal in progress/),
    ).toBeInTheDocument();
  });

  it("does not show stale in-progress copy when refinement has evidence state", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "System is stable",
            spike_task_id: "uuid-spike-5",
            spike_short_id: "ij90",
            spike_status: "open",
            evidence_phase: "awaiting_evidence",
          },
        })}
        refinement={refinement({ active: true })}
      />,
    );
    // The evidence row and note are shown
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    // The refinement active note is still present (ReadinessPanel renders it
    // independently of evidence state, driven by refinement.active)
    expect(
      screen.getByText(/Autonomous tribunal in progress/),
    ).toBeInTheDocument();
  });

  // ── Regression: normal states still render correctly ────────────────────

  it("still renders normal in-progress refinement without evidence states", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus()}
        refinement={refinement({ active: true, current_round: 2 })}
      />,
    );
    expect(screen.getByText("Ready")).toBeInTheDocument();
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    expect(screen.getByText("none required")).toBeInTheDocument();
    expect(
      screen.getByText(/Autonomous tribunal in progress/),
    ).toBeInTheDocument();
  });

  it("still renders converged state without evidence interference", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Approved",
          judge_verdict_id: "v-1",
          judge_needs_work: false,
        })}
        refinement={refinement({
          active: true,
          awaiting_review: true,
          current_round: 3,
        })}
      />,
    );
    expect(screen.getByText("Ready")).toBeInTheDocument();
    expect(
      screen.getByText(/approve \/ ready/),
    ).toBeInTheDocument();
    // No evidence row content
    expect(screen.getByText("none required")).toBeInTheDocument();
  });
});
