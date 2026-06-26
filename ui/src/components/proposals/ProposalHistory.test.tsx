import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@/test/test-utils";
import { fetchUsers, type OrgUser } from "@/api/users";
import type { Proposal, ProposalRevision } from "@/api/types";
import type { ProposalDetail } from "@/lib/proposalQueries";
import { ProposalHistory } from "./ProposalHistory";

vi.mock("@/api/users", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/users")>();
  return {
    ...actual,
    fetchUsers: vi.fn(),
  };
});

vi.mock("@/components/memory/memoryUtils", () => ({
  relativeTime: vi.fn((iso: string) =>
    iso === "2026-06-02T00:00:00Z" ? "2h ago" : "1d ago",
  ),
}));

const users: OrgUser[] = [
  {
    id: "user-1",
    github_login: "pm-user",
    github_name: "Pat Manager",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: true,
    role: "pm",
  },
];

function proposal(overrides: Partial<Proposal> = {}): Proposal {
  return {
    id: "proposal-1",
    short_id: "p1",
    title: "External implementation",
    status: "done",
    body: "Base body",
    acceptance_criteria: [],
    latest_revision_seq: 1,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-02T00:00:00Z",
    ...overrides,
  };
}

function revision(
  seq: number,
  overrides: Partial<ProposalRevision> = {},
): ProposalRevision {
  return {
    id: `rev-${seq}`,
    seq,
    title: "External implementation",
    body: "Base body",
    acceptance_criteria: [],
    event_kind: "spec_revision",
    created_at: "2026-06-01T00:00:00Z",
    edited_by_user_id: "user-1",
    ...overrides,
  };
}

function detail(revisions: ProposalRevision[]): ProposalDetail {
  return {
    proposal: proposal({
      latest_revision_seq: Math.max(
        1,
        ...revisions
          .filter((r) => r.event_kind === "spec_revision")
          .map((r) => r.seq),
      ),
    }),
    targets: [],
    feedback: [],
    revisions,
    signoffs: [],
    epics: [],
    debate_trail: [],
  };
}

describe("ProposalHistory", () => {
  beforeEach(() => {
    vi.mocked(fetchUsers).mockReset();
    vi.mocked(fetchUsers).mockResolvedValue(users);
  });

  it("renders a manual done status event with a seed revision", async () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1),
          revision(1, {
            id: "status-1",
            event_kind: "status_change",
            status_from: "draft",
            status_to: "done",
            created_at: "2026-06-02T00:00:00Z",
          }),
        ])}
      />,
    );

    expect(screen.getByText("Revision history")).toBeInTheDocument();
    expect(
      screen.getByText("Marked done (implemented externally)"),
    ).toBeInTheDocument();
    expect(screen.getByText("draft")).toBeInTheDocument();
    expect(screen.getByText("done")).toBeInTheDocument();
    expect(screen.getByText("2h ago")).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getAllByText("Pat Manager").length).toBeGreaterThan(0),
    );

    expect(screen.queryByText("rev 0")).not.toBeInTheDocument();
    expect(screen.queryByText("Initial spec.")).not.toBeInTheDocument();
  });

  it("keeps rendering expandable spec revision diffs", async () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1, {
            title: "Original proposal",
            body: "Base body",
          }),
          revision(2, {
            title: "Updated proposal",
            body: "Base body\nAdd rollout notes.",
            created_at: "2026-06-02T00:00:00Z",
          }),
        ])}
      />,
    );

    expect(screen.getByText("rev 2")).toBeInTheDocument();
    expect(screen.getByText("Base body Add rollout notes.")).toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));

    expect(screen.getAllByText("Add rollout notes.").length).toBeGreaterThan(0);
    expect(screen.getByText("Original proposal")).toBeInTheDocument();
    expect(screen.getByText("Updated proposal")).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getAllByText("Pat Manager").length).toBeGreaterThan(0),
    );
  });

  it("lists MDX revision snapshots with a readable preview and raw diff text", async () => {
    const { container } = render(
      <ProposalHistory
        detail={detail([
          revision(1, {
            body_format: "mdx",
            body: [
              "Initial contract.",
              '<ApiEndpoint id="users">',
              "User { id: uuid }",
              "</ApiEndpoint>",
              '<QuestionForm id="open-questions">',
              "- Is email required?",
              "</QuestionForm>",
            ].join("\n"),
          }),
          revision(2, {
            body_format: "mdx",
            body: [
              "Initial contract.",
              '<ApiEndpoint id="users">',
              "User { id: uuid, email: string }",
              "</ApiEndpoint>",
              '<QuestionForm id="open-questions">',
              "- Is email required?",
              "- Who owns retention?",
              "</QuestionForm>",
            ].join("\n"),
            created_at: "2026-06-02T00:00:00Z",
          }),
        ])}
      />,
    );

    expect(screen.getByText("rev 2")).toBeInTheDocument();
    expect(screen.getAllByText("spec_revision")).toHaveLength(2);
    expect(screen.getAllByText("MDX")).toHaveLength(2);
    expect(
      screen.getAllByText(/MDX blocks: ApiEndpoint, QuestionForm/).length,
    ).toBeGreaterThan(0);
    expect(screen.getByTitle("2026-06-02T00:00:00Z")).toBeInTheDocument();
    expect(screen.queryByText("- Who owns retention?")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));

    expect(screen.getByText('<ApiEndpoint id="users">')).toBeInTheDocument();
    expect(
      screen.getByText("User { id: uuid, email: string }"),
    ).toBeInTheDocument();
    expect(screen.getByText("- Who owns retention?")).toBeInTheDocument();
    expect(container.querySelector("ApiEndpoint")).toBeNull();
    expect(container.querySelector("QuestionForm")).toBeNull();
    await waitFor(() =>
      expect(screen.getAllByText("Pat Manager").length).toBeGreaterThan(0),
    );
  });

  // ── P4 regression: control events in history ────────────────────────

  it("renders refinement lifecycle events in proposal history", () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1),
          revision(1, {
            id: "refinement-start",
            event_kind: "status_change",
            status_from: "draft",
            status_to: "in_review",
            created_at: "2026-06-02T00:00:00Z",
          }),
        ])}
      />,
    );

    expect(screen.getByText("Revision history")).toBeInTheDocument();
    expect(screen.getByText("Status changed")).toBeInTheDocument();
    expect(screen.getByText("draft")).toBeInTheDocument();
    expect(screen.getByText("in_review")).toBeInTheDocument();
  });

  it("renders multiple control events alongside spec revisions", () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1),
          revision(2, {
            title: "Updated proposal",
            body: "Base body\nAdd error handling.",
            created_at: "2026-06-02T00:00:00Z",
          }),
          revision(2, {
            id: "status-to-review",
            event_kind: "status_change",
            status_from: "draft",
            status_to: "in_review",
            created_at: "2026-06-02T00:00:00Z",
          }),
          revision(2, {
            id: "status-to-done",
            event_kind: "status_change",
            status_from: "in_review",
            status_to: "done",
            created_at: "2026-06-02T00:00:00Z",
          }),
        ])}
      />,
    );

    // Both status events should be visible: the in_review transition renders
    // as a generic "Status changed", while the transition to "done" renders
    // with the dedicated externally-implemented label.
    expect(screen.getByText("Status changed")).toBeInTheDocument();
    expect(
      screen.getByText("Marked done (implemented externally)"),
    ).toBeInTheDocument();
    // Spec revision should be visible.
    expect(screen.getByText("rev 2")).toBeInTheDocument();
  });

  it("renders verdict_override control event in history", () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1),
          revision(1, {
            id: "verdict-override",
            event_kind: "status_change",
            status_from: "draft",
            status_to: "in_review",
            created_at: "2026-06-02T00:00:00Z",
            event_metadata: JSON.stringify({
              source: "human_verdict_override",
              reason: "PM approved scope as-is",
            }),
          }),
        ])}
      />,
    );

    expect(screen.getByText("Revision history")).toBeInTheDocument();
    expect(screen.getByText("Status changed")).toBeInTheDocument();
  });
});
