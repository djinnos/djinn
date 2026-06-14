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
    expect(screen.queryByText("Add rollout notes.")).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));

    expect(screen.getByText("Add rollout notes.")).toBeInTheDocument();
    expect(screen.getByText("Original proposal")).toBeInTheDocument();
    expect(screen.getByText("Updated proposal")).toBeInTheDocument();
    await waitFor(() =>
      expect(screen.getAllByText("Pat Manager").length).toBeGreaterThan(0),
    );
  });
});
