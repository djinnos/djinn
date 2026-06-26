import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen } from "@/test/test-utils";
import type {
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

  it("renders Ready badge when gate passes", () => {
    render(<ReadinessPanel gateStatus={gateStatus()} refinement={refinement()} />);
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
      />,
    );
    expect(screen.getByText(/Definition of Ready: Fail/)).toBeInTheDocument();
    expect(screen.getByText(/Problem coverage/)).toBeInTheDocument();
    expect(
      screen.getAllByText(/Missing required coverage: problem/).length,
    ).toBeGreaterThan(0);
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
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    // "Ready" appears both as the header readiness badge and the judge badge.
    expect(screen.getAllByText("Ready").length).toBeGreaterThanOrEqual(2);
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
      />,
    );
    expect(screen.getByText("Parked: needs-evidence spike")).toBeInTheDocument();
    expect(screen.getAllByText(/X is load-bearing/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/ab12/).length).toBeGreaterThan(0);
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
            "Judge returned needs-work (verdict v-1); no current human override",
          ],
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Blocked because")).toBeInTheDocument();
    expect(screen.getByText(/Missing required coverage: problem/)).toBeInTheDocument();
    expect(screen.getByText(/Judge returned needs-work/)).toBeInTheDocument();
  });

  // ── Human override badge ────────────────────────────────────────────────

  it("renders Human override badge when human_override_active is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ human_override_active: true })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Human override")).toBeInTheDocument();
  });

  it("does not render Human override badge when human_override_active is false", () => {
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
      <ReadinessPanel gateStatus={gateStatus()} refinement={refinement({ active: true })} />,
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

  // ── P4 regression: needs-evidence with closed spike ──────────────────

  it("renders needs-evidence with closed spike status", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          needs_evidence: {
            claim: "Y handles 10k rps",
            spike_task_id: "uuid-spike-2",
            spike_short_id: "cd34",
            spike_status: "done",
          },
          ready: false,
          blocked_explanations: [
            "Proposal parked on needs-evidence spike cd34 (claim: Y handles 10k rps)",
          ],
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Parked: needs-evidence spike")).toBeInTheDocument();
    expect(screen.getAllByText(/Y handles 10k rps/).length).toBeGreaterThan(0);
    expect(screen.getAllByText(/cd34/).length).toBeGreaterThan(0);
    expect(screen.getByText(/done/)).toBeInTheDocument();
  });

  // ── P4 regression: multiple blocked explanations ─────────────────────

  it("renders multiple blocked explanations simultaneously", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          dor_ready: false,
          dor_failures: [
            { check: "problem_coverage", message: "Missing required coverage: problem" },
            { check: "grounding", message: "Missing grounding: add entry points" },
          ],
          judge_needs_work: true,
          unresolved_blocking_count: 1,
          unresolved_blocking_ids: ["e-1"],
          blocked_explanations: [
            "Missing required coverage: problem",
            "Missing grounding: add entry points",
            "Judge returned needs-work (verdict v-1); no current human override",
            "Unresolved blocking debate entries: e-1",
          ],
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Blocked because")).toBeInTheDocument();
    expect(
      screen.getAllByText(/Missing required coverage: problem/).length,
    ).toBeGreaterThan(0);
    expect(screen.getByText(/Judge returned needs-work/)).toBeInTheDocument();
    expect(screen.getByText(/Unresolved blocking debate entries/)).toBeInTheDocument();
  });

  // ── P4 regression: pending checkpoint badge ──────────────────────────

  it("renders Pending checkpoint badge when pending_checkpoint is true", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({ pending_checkpoint: true })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Pending checkpoint")).toBeInTheDocument();
  });

  // ── P4 regression: Judge Ready verdict with override ─────────────────

  it("shows both judge Ready verdict and human override badge", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          judge_verdict_body: "Looks good",
          judge_verdict_id: "v-3",
          judge_needs_work: false,
          human_override_active: true,
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByText("Judge verdict")).toBeInTheDocument();
    // "Ready" appears as both the header readiness badge and the judge badge.
    expect(screen.getAllByText("Ready").length).toBeGreaterThanOrEqual(2);
    expect(screen.getByText("Human override")).toBeInTheDocument();
  });
});
