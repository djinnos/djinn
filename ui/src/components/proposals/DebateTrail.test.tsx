import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@/test/test-utils";
import type { ProposalDebateTrailRow } from "@/api/types";
import { DebateTrail } from "./DebateTrail";

vi.mock("@/components/memory/memoryUtils", () => ({
  relativeTime: vi.fn((iso: string) => {
    if (iso === "2026-06-20T10:00:00Z") return "4d ago";
    if (iso === "2026-06-20T12:00:00Z") return "3d ago";
    if (iso === "2026-06-21T08:00:00Z") return "2d ago";
    if (iso === "2026-06-22T09:00:00Z") return "1d ago";
    return "just now";
  }),
}));

function row(overrides: Partial<ProposalDebateTrailRow> = {}): ProposalDebateTrailRow {
  return {
    id: "dt-1",
    proposal_id: "p-1",
    kind: "objection",
    body: "Test body",
    blocking: false,
    agent_role: "adversary",
    author_kind: "agent",
    author_user_id: null,
    author_model: "gpt-4o",
    source_task_id: null,
    against_revision_seq: 1,
    round: 1,
    resolved_at: null,
    resolved_by_user_id: null,
    reopened_at: null,
    reopened_by_user_id: null,
    created_at: "2026-06-20T10:00:00Z",
    updated_at: "2026-06-20T10:00:00Z",
    ...overrides,
  };
}

describe("DebateTrail", () => {
  it("renders nothing when debate trail is empty", () => {
    const { container } = render(<DebateTrail debateTrail={[]} />);
    expect(container.firstChild).toBeNull();
  });

  it("groups entries by round with round headers", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({ id: "r1", round: 1, body: "Round 1 objection" }),
          row({ id: "r2", round: 1, kind: "rebuttal", body: "Round 1 rebuttal" }),
          row({ id: "r3", round: 2, body: "Round 2 objection" }),
        ]}
      />,
    );

    expect(screen.getByText("Debate trail")).toBeInTheDocument();
    expect(screen.getByText("Round 1")).toBeInTheDocument();
    expect(screen.getByText("Round 2")).toBeInTheDocument();
    expect(screen.getByText("Round 1 objection")).toBeInTheDocument();
    expect(screen.getByText("Round 1 rebuttal")).toBeInTheDocument();
    expect(screen.getByText("Round 2 objection")).toBeInTheDocument();
  });

  it("renders objection, rebuttal, and verdict kind badges", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({ id: "obj", kind: "objection" }),
          row({ id: "reb", kind: "rebuttal" }),
          row({ id: "ver", kind: "verdict" }),
        ]}
      />,
    );

    expect(screen.getByText("objection")).toBeInTheDocument();
    expect(screen.getByText("rebuttal")).toBeInTheDocument();
    expect(screen.getByText("verdict")).toBeInTheDocument();
  });

  it("shows blocking badge for blocking rows", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({ id: "blocked", blocking: true }),
          row({ id: "open", blocking: false }),
        ]}
      />,
    );

    const blockingBadges = screen.getAllByText("Blocking");
    const nonBlockingBadges = screen.getAllByText("Non-blocking");
    expect(blockingBadges).toHaveLength(1);
    expect(nonBlockingBadges).toHaveLength(1);
  });

  it("shows open state for unresolved entries", () => {
    render(
      <DebateTrail debateTrail={[row({ id: "open", resolved_at: null })]} />,
    );
    expect(screen.getByText("Open")).toBeInTheDocument();
  });

  it("shows resolved state for resolved entries", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({
            id: "resolved",
            resolved_at: "2026-06-21T08:00:00Z",
            resolved_by_user_id: "user-1",
          }),
        ]}
      />,
    );
    expect(screen.getByText("Resolved")).toBeInTheDocument();
  });

  it("shows reopened state for entries that were resolved then reopened", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({
            id: "reopened",
            resolved_at: "2026-06-21T08:00:00Z",
            resolved_by_user_id: "user-1",
            reopened_at: "2026-06-22T09:00:00Z",
            reopened_by_user_id: "user-1",
          }),
        ]}
      />,
    );
    expect(screen.getByText("Reopened")).toBeInTheDocument();
  });

  it("renders agent role badge and author attribution", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({
            id: "adv",
            agent_role: "adversary",
            author_kind: "agent",
            author_model: "gpt-4o",
          }),
        ]}
      />,
    );
    expect(screen.getByText("adversary")).toBeInTheDocument();
    expect(screen.getByText("agent")).toBeInTheDocument();
    expect(screen.getByText("gpt-4o")).toBeInTheDocument();
  });

  it("renders user author attribution", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({
            id: "usr",
            agent_role: "advocate",
            author_kind: "user",
            author_user_id: "user-42",
            author_model: null,
          }),
        ]}
      />,
    );
    expect(screen.getByText("advocate")).toBeInTheDocument();
    expect(screen.getByText("user")).toBeInTheDocument();
    expect(screen.getByText("user-42")).toBeInTheDocument();
  });

  it("renders against_revision_seq badge", () => {
    render(
      <DebateTrail
        debateTrail={[row({ id: "rev-anchor", against_revision_seq: 5 })]}
      />,
    );
    expect(screen.getByText("rev 5")).toBeInTheDocument();
  });

  it("renders timestamps", () => {
    render(
      <DebateTrail
        debateTrail={[row({ id: "ts", created_at: "2026-06-20T10:00:00Z" })]}
      />,
    );
    expect(screen.getByText("4d ago")).toBeInTheDocument();
  });

  it("renders markdown body content", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({ id: "md", body: "**Bold claim** with [link](https://example.com)" }),
        ]}
      />,
    );
    expect(screen.getByText("Bold claim")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "link" })).toBeInTheDocument();
  });

  it("shows round entry count", () => {
    render(
      <DebateTrail
        debateTrail={[
          row({ id: "a", round: 1 }),
          row({ id: "b", round: 1, kind: "rebuttal" }),
        ]}
      />,
    );
    expect(screen.getByText("2 entries")).toBeInTheDocument();
  });

  it("shows singular entry label for single-entry rounds", () => {
    render(
      <DebateTrail debateTrail={[row({ id: "solo", round: 1 })]} />,
    );
    expect(screen.getByText("1 entry")).toBeInTheDocument();
  });
});
