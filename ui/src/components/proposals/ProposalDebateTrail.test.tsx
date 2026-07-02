import { describe, expect, it } from "vitest";
import { render, screen, userEvent } from "@/test/test-utils";
import type { ProposalDebateTrailRow } from "@/api/types";
import { ProposalDebateTrail } from "./ProposalDebateTrail";
import { verdictOutcome } from "./debateTrailUtils";

function row(overrides: Partial<ProposalDebateTrailRow> = {}): ProposalDebateTrailRow {
  return {
    id: `e-${Math.random().toString(36).slice(2)}`,
    proposal_id: "p-1",
    kind: "objection",
    body: "An objection body.",
    blocking: false,
    agent_role: "adversary",
    author_kind: "agent",
    author_user_id: null,
    author_model: "gpt",
    source_task_id: null,
    against_revision_seq: 1,
    round: 1,
    resolved_at: null,
    resolved_by_user_id: null,
    reopened_at: null,
    reopened_by_user_id: null,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-01T00:00:00Z",
    ...overrides,
  };
}

describe("verdictOutcome", () => {
  it("parses an approve verdict from the body", () => {
    expect(
      verdictOutcome(row({ kind: "verdict", body: "Verdict: approve — ready" })),
    ).toEqual({ label: "approve", positive: true });
  });

  it("parses a reject/needs-work verdict from the body", () => {
    expect(
      verdictOutcome(
        row({ kind: "verdict", body: "Verdict: needs-work on scope" }),
      ).positive,
    ).toBe(false);
  });

  it("falls back to the blocking flag when no marker is present", () => {
    expect(
      verdictOutcome(row({ kind: "verdict", body: "no marker", blocking: true }))
        .positive,
    ).toBe(false);
    expect(
      verdictOutcome(row({ kind: "verdict", body: "no marker", blocking: false }))
        .positive,
    ).toBe(true);
  });
});

describe("ProposalDebateTrail", () => {
  it("renders an empty message when there are no entries", () => {
    render(<ProposalDebateTrail trail={[]} />);
    expect(screen.getByText("No debate entries yet.")).toBeInTheDocument();
  });

  it("groups by round with the latest round expanded and earlier ones collapsed", () => {
    render(
      <ProposalDebateTrail
        trail={[
          row({
            id: "r1-o1",
            round: 1,
            against_revision_seq: 1,
            blocking: true,
            resolved_at: "2026-06-01T01:00:00Z",
            body: "Round 1 objection.",
          }),
          row({
            id: "r1-v",
            round: 1,
            kind: "verdict",
            agent_role: "judge",
            against_revision_seq: 1,
            body: "Verdict: reject",
          }),
          row({
            id: "r2-o1",
            round: 2,
            against_revision_seq: 2,
            body: "Round 2 objection.",
          }),
          row({
            id: "r2-v",
            round: 2,
            kind: "verdict",
            agent_role: "judge",
            against_revision_seq: 2,
            body: "Verdict: approve",
          }),
        ]}
      />,
    );

    expect(screen.getByText("Round 1")).toBeInTheDocument();
    expect(screen.getByText("Round 2")).toBeInTheDocument();

    // Round summaries carry the objection + verdict outcome.
    expect(screen.getByText(/1 objection \(1 ✓ resolved\)/)).toBeInTheDocument();

    // Latest round (2) is expanded by default → its entry body is visible.
    expect(screen.getByText("Round 2 objection.")).toBeInTheDocument();
    // Round 1 is collapsed → its entry body is not visible yet.
    expect(screen.queryByText("Round 1 objection.")).not.toBeInTheDocument();
  });

  it("expands a collapsed round on click", async () => {
    const user = userEvent.setup();
    render(
      <ProposalDebateTrail
        trail={[
          row({ id: "r1", round: 1, body: "Round 1 objection." }),
          row({ id: "r2", round: 2, body: "Round 2 objection." }),
        ]}
      />,
    );

    expect(screen.queryByText("Round 1 objection.")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /Round 1/ }));
    expect(screen.getByText("Round 1 objection.")).toBeInTheDocument();
  });

  it("shows a rev range when entries span multiple revisions", () => {
    render(
      <ProposalDebateTrail
        trail={[
          row({ id: "a", round: 3, against_revision_seq: 3 }),
          row({ id: "b", round: 3, against_revision_seq: 4 }),
        ]}
      />,
    );
    expect(screen.getByText("vs rev 3→4")).toBeInTheDocument();
  });

  it("expands a long entry body to full markdown", async () => {
    const user = userEvent.setup();
    const longBody =
      "## Objection\n\n" + "This is a very long objection body. ".repeat(10);
    render(
      <ProposalDebateTrail
        trail={[row({ id: "long", round: 1, body: longBody })]}
      />,
    );

    // The round is latest → expanded; the entry offers an Expand control.
    const expand = screen.getByRole("button", { name: "Expand" });
    await user.click(expand);
    expect(
      screen.getByRole("heading", { name: "Objection" }),
    ).toBeInTheDocument();
  });
});
