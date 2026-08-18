import { useState } from "react";
import { describe, expect, it, vi, beforeEach } from "vitest";
import { render, screen, userEvent, waitFor } from "@/test/test-utils";
import type {
  ProposalDebateTrailRow,
  ProposalGateStatus,
  ProposalRefinementStatus,
  TypedEvidenceGateStatus,
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

/**
 * Drives a real interaction against every enabled control the panel currently
 * renders, and returns how many it clicked.
 *
 * This is what makes the `expect(callMcpTool).not.toHaveBeenCalled()`
 * assertions below falsifiable. A render-only test cannot issue a mutation no
 * matter what the component does, so a zero-call assertion after it holds
 * trivially. With the typed-evidence retry visibility guard removed, the retry
 * button *is* rendered — this sweep then clicks it and the mutation fires, so
 * every zero-call assertion in this file goes red. The positive control in
 * "issues zero calls with typed presentation hidden" proves the sweep really
 * does fire the mutation when the control is present.
 */
const CONTROL_SELECTOR = [
  "button",
  '[role="button"]',
  "a[href]",
  '[role="link"]',
  '[role="menuitem"]',
  '[role="switch"]',
  '[role="tab"]',
  'input[type="button"]',
  'input[type="submit"]',
  "summary",
].join(",");

/** Whether a control would accept a real user's click. */
function isInteractable(element: Element): boolean {
  return (
    !element.hasAttribute("disabled") &&
    element.getAttribute("aria-disabled") !== "true"
  );
}

async function clickEveryRenderedControl(): Promise<number> {
  const clicked = new Set<Element>();
  // Re-query between rounds rather than snapshotting once. A two-step control
  // — click, confirm dialog, confirm — only renders its second button after
  // the first is pressed, so a list taken before the first click could never
  // reach the button that actually issues the mutation. The bound stops a
  // control that re-renders itself forever.
  for (let round = 0; round < 6; round += 1) {
    const pending = Array.from(
      document.body.querySelectorAll(CONTROL_SELECTOR),
    ).filter((element) => !clicked.has(element) && isInteractable(element));
    if (pending.length === 0) break;
    for (const control of pending) {
      clicked.add(control);
      // A click earlier in this round may have unmounted it.
      if (!control.isConnected) continue;
      await userEvent.click(control);
    }
  }
  return clicked.size;
}

/**
 * The accessible names of every control rendered inside the typed-evidence
 * finding, sorted.
 *
 * This is what replaced `FORBIDDEN_ACTION_LABELS`, a blocklist of six labels
 * ("Resolve evidence", "Withdraw demand", …) that no component in `ui/src` has
 * ever rendered. A blocklist can only catch a name someone thought to write
 * down in advance; asserting the whole set catches any control at all,
 * whatever it is called and whichever element it is rendered as.
 */
function typedEvidenceControlNames(): string[] {
  const finding = screen.queryByTestId("typed-evidence-finding");
  if (finding === null) return [];
  return Array.from(finding.querySelectorAll(CONTROL_SELECTOR))
    .filter(isInteractable)
    .map(
      (element) =>
        (element.textContent ?? "").trim() ||
        element.getAttribute("aria-label") ||
        "<control with no accessible name>",
    )
    .sort();
}

// The sweep's own tests.
//
// `clickEveryRenderedControl` is what makes every
// `expect(callMcpTool).not.toHaveBeenCalled()` below falsifiable, so it needs
// tests of its own: a sweep that quietly stops reaching controls turns a whole
// file of zero-call assertions into decoration without a single test going red.
//
// Both cases here are defects adversarial verification found in the first
// version: it matched only `button:not([disabled])`, and it snapshotted the
// control list once, before the first click.

describe("clickEveryRenderedControl", () => {
  it("reaches a role=button div and an anchor, not only <button>", async () => {
    const fired: string[] = [];
    render(
      <div>
        <button onClick={() => fired.push("button")}>Real button</button>
        <div role="button" tabIndex={0} onClick={() => fired.push("role")}>
          Div with a button role
        </div>
        <a href="#somewhere" onClick={() => fired.push("anchor")}>
          Anchor
        </a>
        <button disabled onClick={() => fired.push("disabled")}>
          Disabled
        </button>
        <div
          aria-disabled="true"
          role="button"
          onClick={() => fired.push("aria")}
        >
          Aria-disabled
        </div>
      </div>,
    );

    const clicked = await clickEveryRenderedControl();

    expect(fired.sort()).toEqual(["anchor", "button", "role"]);
    expect(clicked).toBe(3);
  });

  it("reaches the second step of a two-step control", async () => {
    // The control that matters is the one that only exists AFTER the first
    // click. A sweep that takes its list up front can never press it, so a
    // destructive two-step affordance would pass every zero-call assertion in
    // this file.
    const fired: string[] = [];
    function TwoStep() {
      const [open, setOpen] = useState(false);
      return (
        <div>
          <button onClick={() => setOpen(true)}>Open</button>
          {open && (
            <button onClick={() => fired.push("confirmed")}>Confirm</button>
          )}
        </div>
      );
    }
    render(<TwoStep />);

    const clicked = await clickEveryRenderedControl();

    expect(fired).toEqual(["confirmed"]);
    expect(clicked).toBe(2);
  });
});

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
    author_user_id: undefined,
    author_model: "gpt",
    source_task_id: undefined,
    against_revision_seq: 3,
    round: 2,
    resolved_at: undefined,
    resolved_by_user_id: undefined,
    reopened_at: undefined,
    reopened_by_user_id: undefined,
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
    expect(screen.getByText("Awaiting evidence: ab12")).toBeInTheDocument();
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

  // ── Evidence lifecycle matrix (task 787p) ────────────────────────────────

  it("renders awaiting-evidence from status lifecycle with spike and claim summary", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: null,
        })}
        refinement={refinement({
          active: true,
          evidence_lifecycle_state: "awaiting_evidence",
          needs_evidence: {
            claim: "Can the Rust proc-macro crate emit JSON?",
            spike_task_id: "11111111-1111-1111-1111-111111111111",
            spike_short_id: "sp-1",
            spike_status: "in_progress",
            evidence_phase: "awaiting_evidence",
            question: "Can the Rust proc-macro crate emit JSON?",
            round: 2,
          },
        })}
      />,
    );
    // Gate row uses the status-level evidence (gate was null).
    expect(
      screen.getByText(/awaiting evidence — spike sp-1 \(in_progress\)/),
    ).toBeInTheDocument();
    expect(
      screen.getAllByText("Can the Rust proc-macro crate emit JSON?").length,
    ).toBeGreaterThanOrEqual(1);
    // Awaiting-evidence note with short id and spike id.
    expect(screen.getByText("Awaiting evidence: sp-1")).toBeInTheDocument();
    expect(
      screen.getByText("11111111-1111-1111-1111-111111111111"),
    ).toBeInTheDocument();
    // Should NOT show generic in-progress copy.
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
  });

  it("renders awaiting-evidence from gate status when status lacks needs_evidence", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Fallback gate claim",
            spike_task_id: "22222222-2222-2222-2222-222222222222",
            spike_short_id: "sp-gate",
            spike_status: "open",
            evidence_phase: "awaiting_evidence",
            question: "Fallback gate question",
          },
        })}
        refinement={refinement({ active: true })}
      />,
    );
    expect(
      screen.getByText(/awaiting evidence — spike sp-gate \(open\)/),
    ).toBeInTheDocument();
    expect(screen.getAllByText("Fallback gate question").length).toBeGreaterThanOrEqual(1);
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
  });

  it("renders evidence-failed with failure reason and blocked copy", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Y handles concurrency safely",
            spike_task_id: "33333333-3333-3333-3333-333333333333",
            spike_short_id: "cd34",
            spike_status: "closed",
            evidence_phase: "evidence_failed",
            failure_reason: "spike_force_closed",
          },
        })}
        refinement={refinement({ active: true })}
      />,
    );
    // Gate row shows failed with reason.
    expect(
      screen.getByText(/evidence failed — spike cd34 \(spike_force_closed\)/),
    ).toBeInTheDocument();
    // Destructive note with blocked copy and reason.
    expect(screen.getByText("Evidence failed: cd34")).toBeInTheDocument();
    expect(
      screen.getByText(/Refinement is blocked on failed or missing evidence findings/),
    ).toBeInTheDocument();
    expect(screen.getByText(/Reason: spike_force_closed/)).toBeInTheDocument();
    // No contradictory in-progress note.
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
    // No amber "awaiting" note.
    expect(
      screen.queryByText(/Awaiting evidence:/),
    ).not.toBeInTheDocument();
  });

  it("renders ordinary in-progress copy only when no evidence lifecycle applies", () => {
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
        refinement={refinement({ active: true })}
      />,
    );
    // Implicit awaiting evidence keeps the in-progress copy suppressed.
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
    // It still shows an awaiting-evidence note.
    expect(screen.getByText("Awaiting evidence: ab12")).toBeInTheDocument();
  });

  it("does not duplicate converged/awaiting-review as an evidence state", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: true,
          needs_evidence: {
            claim: "Should be ignored because awaiting review",
            spike_task_id: "44444444-4444-4444-4444-444444444444",
            spike_short_id: "sp-ar",
            spike_status: "in_progress",
            evidence_phase: "awaiting_evidence",
          },
        })}
        refinement={refinement({
          active: false,
          awaiting_review: true,
          current_round: 3,
        })}
      />,
    );
    expect(screen.getByText("Ready")).toBeInTheDocument();
    // The evidence checklist row still reflects the gate payload, but the
    // awaiting-review state is not duplicated as a distinct evidence lifecycle
    // note (and no automatic-resume / in-progress copy appears).
    expect(
      screen.getByText(/awaiting evidence — spike sp-ar \(in_progress\)/),
    ).toBeInTheDocument();
    // No evidence note should render.
    expect(
      screen.queryByText(/Awaiting evidence:/),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText(/Evidence failed:/),
    ).not.toBeInTheDocument();
    // No in-progress note since not active.
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
  });

  it("renders paused/frozen with precedence and no auto-resume language", () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Z is safe to deploy",
            spike_task_id: "55555555-5555-5555-5555-555555555555",
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
    // Paused/frozen note.
    expect(screen.getByText("Paused or frozen")).toBeInTheDocument();
    expect(
      screen.getByText(/Refinement is paused or frozen manually/),
    ).toBeInTheDocument();
    // Evidence context preserved but no auto-resume wording.
    expect(screen.getAllByText(/Z is safe to deploy/).length).toBeGreaterThanOrEqual(1);
    expect(
      screen.queryByText(/Autonomous tribunal in progress/),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByText(/refinement can resume/),
    ).not.toBeInTheDocument();
  });

  it("falls back to gate evidence when refinement is null and gate has awaiting evidence", () => {
    const { container } = render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Gate-only claim",
            spike_task_id: "66666666-6666-6666-6666-666666666666",
            spike_short_id: "sp-only",
            spike_status: "open",
            evidence_phase: "awaiting_evidence",
          },
        })}
        refinement={null}
      />,
    );
    // The panel renders because gateStatus is present.
    expect(container.innerHTML).not.toBe("");
    expect(screen.getByText("Readiness gate")).toBeInTheDocument();
    expect(screen.getByText("Blocked")).toBeInTheDocument();
    expect(screen.getByText("Evidence spike")).toBeInTheDocument();
    // When refinement is null the classifier returns not_started, so the panel
    // does not render evidence lifecycle notes; it still renders the checklist.
    expect(screen.getByText("none required")).toBeInTheDocument();
    expect(
      screen.queryByText(/Awaiting evidence:/),
    ).not.toBeInTheDocument();
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

// ── Typed evidence finding detail (menk) ─────────────────────────────────────
//
// The panel renders the server's typed projection whole. Every assertion below
// is on a value the server sent; nothing the panel derives locally is asserted,
// because there is nothing it may derive locally.

/** A complete typed section with sensible defaults. */
function typedEvidence(
  overrides: Partial<TypedEvidenceGateStatus> = {},
): TypedEvidenceGateStatus {
  return {
    mode: "enforce",
    blocking: true,
    finding_id: "finding-abc",
    claim: '{"question":"Can the launcher share a cgroup across pods?"}',
    lifecycle: "demanded",
    demanded_revision_seq: 2,
    attempt_seq: 1,
    attempts: [],
    planned_checks: [],
    gaps: [],
    usable_findings: [],
    retry_permitted: false,
    // A finding that already has a failed attempt behind it is the ordinary
    // case, and it is the one that makes the zero-call assertions in this file
    // mean something: with a failed transition present, `retry_permitted` is
    // the ONLY thing standing between each panel and a rendered, clickable
    // mutation. Default it to absent and most of these panels are two guards
    // away from a control, so the sweep finds nothing to click no matter what
    // the component does.
    failed_transition_id: "transition-9",
    ...overrides,
  } as TypedEvidenceGateStatus;
}

const TYPED_LIFECYCLES = [
  "demanded",
  "spike_active",
  "evidence_received",
  "failed",
  "resolved",
  "withdrawn",
] as const;

describe("ReadinessPanel typed evidence finding", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
  });

  it("renders the finding for each of the six lifecycle states", async () => {
    for (const lifecycle of TYPED_LIFECYCLES) {
      const blocking = !["resolved", "withdrawn"].includes(lifecycle);
      const { unmount } = render(
        <ReadinessPanel
          gateStatus={gateStatus({
            ready: !blocking,
            typed_evidence: typedEvidence({ lifecycle, blocking }),
          })}
          refinement={refinement()}
        />,
      );
      const card = screen.getByTestId("typed-evidence-finding");
      // The lifecycle is rendered as the server's own token, not a re-labelled
      // approximation, so a reader can match it against the durable state.
      expect(card, lifecycle).toHaveTextContent(lifecycle);
      expect(card).toHaveTextContent("finding-abc");
      expect(card).toHaveTextContent(
        "Can the launcher share a cgroup across pods?",
      );
      // Blocking is the server's flag, and only the four unresolved states
      // carry it in this fixture.
      expect(
        card.textContent?.includes("Blocking"),
        `${lifecycle} blocking badge`,
      ).toBe(blocking);
      await clickEveryRenderedControl();
      unmount();
    }
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("renders all three evidence outcomes, and says so when none has landed", async () => {
    for (const outcome of ["resolved", "partial", "unresolved"] as const) {
      const { unmount } = render(
        <ReadinessPanel
          gateStatus={gateStatus({
            ready: false,
            typed_evidence: typedEvidence({
              lifecycle: "evidence_received",
              evidence_outcome: outcome,
            }),
          })}
          refinement={refinement()}
        />,
      );
      expect(screen.getByTestId("typed-evidence-finding")).toHaveTextContent(
        outcome,
      );
      await clickEveryRenderedControl();
      unmount();
    }
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({ lifecycle: "demanded" }),
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByTestId("typed-evidence-finding")).toHaveTextContent(
      "no validated return yet",
    );
    // This fourth panel used to be asserted about without ever being swept, so
    // the zero-call assertion below held trivially for it.
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("distinguishes a healthy anchor from an unusable, method-incompatible one", async () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({
            lifecycle: "evidence_received",
            evidence_outcome: "partial",
            planned_checks: [
              {
                check_id: "cgroup-delegation",
                method: "code",
                status: "passed",
                anchor_locator: "code://server/src/launcher.rs#L42",
                anchor_health: "healthy",
              },
              {
                check_id: "graph-reachability",
                method: "graph",
                status: "failed",
                anchor_locator: "graph://generation/9f2",
                anchor_health: "unusable",
              },
              {
                // A check with nothing returned yet omits these three fields
                // on the wire: the server marks them
                // `skip_serializing_if = "Option::is_none"`
                // (`proposal_ops.rs`, `TypedEvidencePlannedCheckModel`), so
                // `null` is a value this projection can never carry.
                check_id: "not-yet-run",
                method: "command",
                status: undefined,
                anchor_locator: undefined,
                anchor_health: undefined,
              },
            ],
          }),
        })}
        refinement={refinement()}
      />,
    );
    const checks = screen.getAllByTestId("typed-evidence-check");
    expect(checks).toHaveLength(3);
    expect(checks[0]).toHaveTextContent("method code");
    expect(checks[0]).toHaveTextContent("anchor healthy");
    // Immutable provenance is rendered, not summarized away.
    expect(checks[0]).toHaveTextContent("code://server/src/launcher.rs#L42");
    // The server derived `unusable`; the panel says which method was not
    // server-compatible rather than leaving the reader to guess.
    expect(checks[1]).toHaveTextContent("anchor unusable");
    expect(checks[1]).toHaveTextContent(
      "method graph is not server-compatible for this anchor",
    );
    expect(checks[1]).toHaveTextContent("graph://generation/9f2");
    // A check with no return yet says so instead of rendering a blank cell.
    expect(checks[2]).toHaveTextContent("not returned");
    expect(
      screen.getAllByTestId("typed-evidence-anchor-health"),
    ).toHaveLength(2);
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("renders a finding with attempts, failures, gaps, usable findings and a Judge disposition", async () => {
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          blocked_explanations: [
            "Unresolved typed evidence finding finding-abc (lifecycle: failed; demanded against revision 2)",
          ],
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            evidence_outcome: "unresolved",
            failure_detail: "malformed_findings: conclusion 2 has no anchor",
            folding_revision: 4,
            attempts: [
              {
                sequence: 1,
                spike_task_id: "spike-one",
                outcome: "unresolved",
                failure_detail: "anchor hydration failed",
              },
              { sequence: 2, spike_task_id: "spike-two" },
            ],
            usable_findings: [
              "cgroup.procs is writable by the delegated subtree",
            ],
            gaps: [
              "failure ANCHOR_UNUSABLE: graph anchor could not be dereferenced",
              "gap MISSING_CHECK: no command check was returned",
            ],
            judge_disposition: {
              disposition: "withdrawn",
              outcome: "partial",
              folding_revision: 5,
              judge_task_id: "judge-task-1",
              rationale: "The remaining uncertainty is not load-bearing.",
            },
          }),
        })}
        refinement={refinement()}
      />,
    );
    const attempts = screen.getAllByTestId("typed-evidence-attempt");
    expect(attempts).toHaveLength(2);
    expect(attempts[0]).toHaveTextContent("spike-one");
    expect(attempts[0]).toHaveTextContent("anchor hydration failed");
    expect(attempts[1]).toHaveTextContent("spike-two");

    const gaps = screen.getAllByTestId("typed-evidence-gap");
    expect(gaps).toHaveLength(2);
    expect(gaps[0]).toHaveTextContent("ANCHOR_UNUSABLE");
    expect(gaps[1]).toHaveTextContent("MISSING_CHECK");

    expect(screen.getByTestId("typed-evidence-usable-finding")).toHaveTextContent(
      "cgroup.procs is writable by the delegated subtree",
    );

    const card = screen.getByTestId("typed-evidence-finding");
    expect(card).toHaveTextContent(
      "malformed_findings: conclusion 2 has no anchor",
    );
    expect(card).toHaveTextContent("revision 4");

    const disposition = screen.getByTestId("typed-evidence-disposition");
    expect(disposition).toHaveTextContent("withdrawn");
    expect(disposition).toHaveTextContent("partial");
    expect(disposition).toHaveTextContent("judge-task-1");
    expect(disposition).toHaveTextContent(
      "The remaining uncertainty is not load-bearing.",
    );

    // The server's explanation is rendered verbatim, not paraphrased.
    expect(
      screen.getByTestId("typed-evidence-blocked-explanation"),
    ).toHaveTextContent(
      "Unresolved typed evidence finding finding-abc (lifecycle: failed; demanded against revision 2)",
    );
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("renders identical blocking diagnostics as the proposal head advances to N+1 and N+2", async () => {
    // The finding was demanded against revision 2 and keeps blocking as the
    // head moves. If the panel recomputed staleness against the trail's latest
    // revision it would change what it renders here; it must not.
    const typed = typedEvidence({
      lifecycle: "failed",
      evidence_outcome: "unresolved",
      failure_detail: "malformed_findings",
      demanded_revision_seq: 2,
      attempts: [{ sequence: 1, spike_task_id: "spike-one" }],
      gaps: ["failure ANCHOR_UNUSABLE: graph anchor could not be dereferenced"],
    });
    const explanations = [
      "Unresolved typed evidence finding finding-abc (lifecycle: failed; demanded against revision 2)",
    ];
    const rendered: string[] = [];
    for (const headRevision of [2, 3, 4]) {
      const { unmount } = render(
        <ReadinessPanel
          gateStatus={gateStatus({
            ready: false,
            blocked_explanations: explanations,
            typed_evidence: typed,
          })}
          refinement={refinement()}
          debateTrail={[
            debateRow({
              id: `verdict-${headRevision}`,
              kind: "verdict",
              blocking: false,
              against_revision_seq: headRevision,
              round: headRevision,
            }),
          ]}
        />,
      );
      rendered.push(screen.getByTestId("typed-evidence-finding").innerHTML);
      await clickEveryRenderedControl();
      unmount();
    }
    expect(rendered[0]).toBe(rendered[1]);
    expect(rendered[1]).toBe(rendered[2]);
    // And it still says revision 2 at head 4 — provenance, not a filter.
    expect(rendered[2]).toContain("revision 2");
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("renders no typed section at all when the server published none", async () => {
    const { container } = render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          needs_evidence: {
            claim: "Legacy claim",
            spike_task_id: "legacy-task",
            spike_short_id: "lg-1",
            spike_status: "in_progress",
          },
        })}
        refinement={refinement({
          evidence_lifecycle_state: "awaiting_evidence",
        })}
      />,
    );
    expect(screen.queryByTestId("typed-evidence-finding")).toBeNull();
    // The legacy rendering is exactly what it was: the awaiting-evidence note.
    expect(screen.getByText("Awaiting evidence: lg-1")).toBeInTheDocument();
    expect(container.innerHTML).not.toContain("Typed evidence finding");
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("suppresses the typed section when the projection carries no finding id", async () => {
    // A parity-mismatch projection reports a mode and a reason but no finding.
    // There is nothing to render, and rendering an empty card would read as a
    // finding that exists.
    render(
      <ReadinessPanel
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: {
            mode: "enforce",
            blocking: true,
            parity_mismatch_reason: "typed_evidence_parity_mismatch",
            attempts: [],
            planned_checks: [],
            gaps: [],
            usable_findings: [],
            retry_permitted: false,
          } as unknown as TypedEvidenceGateStatus,
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.queryByTestId("typed-evidence-finding")).toBeNull();
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });
});

// ── Authorized retry action and rollback-safe hiding (gzw2) ──────────────────
//
// The retry control is the panel's only typed-evidence mutation. Its visibility
// comes from the server projection alone, and its arguments are the server's
// own `finding_id` and `failed_transition_id` — the write path admits exactly
// the latest failed transition and rejects anything else, so a browser-derived
// argument would be a button that always fails.

const RETRY_LABEL = "Retry evidence";

describe("ReadinessPanel typed evidence retry action", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
    vi.mocked(showToast.success).mockClear();
    vi.mocked(showToast.error).mockClear();
  });

  it("hides retry for every lifecycle other than failed, even when permitted", async () => {
    for (const lifecycle of TYPED_LIFECYCLES) {
      if (lifecycle === "failed") continue;
      const { unmount } = render(
        <ReadinessPanel
          proposalId="p-1"
          gateStatus={gateStatus({
            ready: false,
            typed_evidence: typedEvidence({
              lifecycle,
              // Both other halves granted: only the lifecycle withholds it.
              retry_permitted: true,
              failed_transition_id: "transition-1",
            }),
          })}
          refinement={refinement()}
        />,
      );
      expect(screen.getByTestId("typed-evidence-finding")).toBeInTheDocument();
      expect(typedEvidenceControlNames(), lifecycle).toEqual([]);
      await clickEveryRenderedControl();
      unmount();
    }
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("hides retry for a failed finding the server does not permit this caller to retry", async () => {
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            retry_permitted: false,
            failed_transition_id: "transition-1",
          }),
        })}
        refinement={refinement()}
      />,
    );
    expect(screen.getByTestId("typed-evidence-finding")).toBeInTheDocument();
    expect(typedEvidenceControlNames()).toEqual([]);
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("hides retry when permission is granted but the server named no failed transition", async () => {
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            retry_permitted: true,
            failed_transition_id: undefined,
          }),
        })}
        refinement={refinement()}
      />,
    );
    expect(typedEvidenceControlNames()).toEqual([]);
    await clickEveryRenderedControl();
    expect(callMcpTool).not.toHaveBeenCalled();
  });

  it("issues exactly one retry call with the server-supplied ids and then invalidates", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({ accepted: true });
    const onChanged = vi.fn();
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            evidence_outcome: "unresolved",
            retry_permitted: true,
            finding_id: "finding-xyz",
            failed_transition_id: "transition-42",
          }),
        })}
        refinement={refinement()}
        onChanged={onChanged}
      />,
    );
    const button = screen.getByRole("button", { name: RETRY_LABEL });
    await userEvent.click(button);

    await waitFor(() => expect(onChanged).toHaveBeenCalledTimes(1));
    expect(callMcpTool).toHaveBeenCalledTimes(1);
    expect(callMcpTool).toHaveBeenCalledWith(
      "proposal_refinement_retry_evidence",
      {
        finding_id: "finding-xyz",
        failed_transition_id: "transition-42",
      },
    );
    expect(showToast.success).toHaveBeenCalledTimes(1);
    expect(showToast.error).not.toHaveBeenCalled();
  });

  it("does not invalidate when the retry is refused", async () => {
    vi.mocked(callMcpTool).mockResolvedValue({
      accepted: false,
      error: "retry_requires_latest_failed_transition",
    });
    const onChanged = vi.fn();
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={gateStatus({
          ready: false,
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            retry_permitted: true,
            failed_transition_id: "transition-stale",
          }),
        })}
        refinement={refinement()}
        onChanged={onChanged}
      />,
    );
    await userEvent.click(screen.getByRole("button", { name: RETRY_LABEL }));
    await waitFor(() => expect(showToast.error).toHaveBeenCalledTimes(1));
    expect(callMcpTool).toHaveBeenCalledTimes(1);
    expect(onChanged).not.toHaveBeenCalled();
  });

  it("renders no control but the authorized retry anywhere in the matrix", async () => {
    // Every cell of the matrix is asserted twice over: its rendered control set
    // must be exactly what that cell is entitled to, and every cell is then
    // swept with real clicks so the tool names collected at the end are the
    // complete set of mutations the typed section can issue.
    //
    // Both assertions used to be weaker. The control check was a blocklist of
    // six labels no component has ever rendered, and beside it
    // `expect(expectedRetryCells).toEqual(["failed/true/true"])` restated the
    // test's own `if` over its own loop variables — it never touched the DOM
    // and survived every mutation an adversarial round threw at it.
    for (const lifecycle of TYPED_LIFECYCLES) {
      for (const permitted of [false, true]) {
        for (const hasTransition of [false, true]) {
          const { unmount } = render(
            <ReadinessPanel
              proposalId="p-1"
              gateStatus={gateStatus({
                ready: false,
                typed_evidence: typedEvidence({
                  lifecycle,
                  retry_permitted: permitted,
                  failed_transition_id: hasTransition
                    ? "transition-1"
                    : undefined,
                  judge_disposition: {
                    disposition: "resolved",
                    outcome: "resolved",
                    folding_revision: 5,
                    judge_task_id: "judge-1",
                    rationale: "Folded into revision 5.",
                  },
                }),
              })}
              refinement={refinement()}
            />,
          );
          const label = `${lifecycle}/${permitted}/${hasTransition}`;
          // Read out of the DOM, not out of the loop variables: exactly one
          // cell may render exactly one control, and every other cell must
          // render none at all.
          expect(typedEvidenceControlNames(), label).toEqual(
            lifecycle === "failed" && permitted && hasTransition
              ? [RETRY_LABEL]
              : [],
          );
          // The Judge disposition is rendered as a recorded server fact, and
          // it still comes with no control to change it.
          expect(
            screen.getByTestId("typed-evidence-disposition"),
          ).toHaveTextContent("Folded into revision 5.");
          await clickEveryRenderedControl();
          unmount();
        }
      }
    }
    // One cell — failed + permitted + a named failed transition — legitimately
    // renders the retry control, and the sweep fires it. Every other cell must
    // contribute nothing at all.
    expect(
      vi.mocked(callMcpTool).mock.calls.map(([tool]) => tool),
    ).toEqual(["proposal_refinement_retry_evidence"]);
  });

  it("issues zero calls with typed presentation hidden, and leaves legacy rendering unchanged", async () => {
    // AC4, in its checkable form. `hiding typed presentation must not strand an
    // active task` is a server-side property no UI test can observe; what the
    // browser CAN be held to is that it issues no mutation at all when the
    // typed section is absent. Server-side stranding is covered by nyfd/115i.
    const legacyGate = gateStatus({
      ready: false,
      blocked_explanations: ["Proposal parked on needs-evidence spike lg-1"],
      needs_evidence: {
        claim: "Legacy claim",
        spike_task_id: "legacy-task-id",
        spike_short_id: "lg-1",
        spike_status: "in_progress",
      },
    });
    const legacyRefinement = refinement({
      evidence_lifecycle_state: "awaiting_evidence",
    });

    const withTyped = render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={{
          ...legacyGate,
          typed_evidence: typedEvidence({
            lifecycle: "failed",
            retry_permitted: true,
            failed_transition_id: "transition-1",
          }),
        }}
        refinement={legacyRefinement}
      />,
    );
    expect(screen.getByRole("button", { name: RETRY_LABEL })).toBeInTheDocument();
    const legacyNoteWithTyped = screen.getByText(
      "Awaiting evidence: lg-1",
    ).parentElement!.innerHTML;
    // Positive control for the sweep used by every zero-call assertion in this
    // file: with the retry control rendered, that same interaction DOES issue
    // the mutation. So a later zero-call assertion after the same sweep is a
    // statement about the component, not about the test doing nothing.
    expect(await clickEveryRenderedControl()).toBeGreaterThan(0);
    await waitFor(() =>
      expect(callMcpTool).toHaveBeenCalledWith(
        "proposal_refinement_retry_evidence",
        { finding_id: "finding-abc", failed_transition_id: "transition-1" },
      ),
    );
    withTyped.unmount();
    vi.mocked(callMcpTool).mockReset();

    // Same proposal with the typed presentation hidden — the exact rollback
    // shape, since `typed_evidence` is absent whenever the rollout mode is
    // `off`.
    render(
      <ReadinessPanel
        proposalId="p-1"
        gateStatus={legacyGate}
        refinement={legacyRefinement}
      />,
    );
    expect(screen.queryByTestId("typed-evidence-finding")).toBeNull();
    expect(screen.queryByRole("button", { name: RETRY_LABEL })).toBeNull();
    // The legacy note is byte-identical to what it was with typed present.
    expect(
      screen.getByText("Awaiting evidence: lg-1").parentElement!.innerHTML,
    ).toBe(legacyNoteWithTyped);
    await clickEveryRenderedControl();
    // The whole point: zero mutations issued.
    expect(callMcpTool).toHaveBeenCalledTimes(0);
  });
});
