import { beforeEach, describe, expect, it, vi } from "vitest";
import { Route, Routes } from "react-router-dom";
import { callMcpTool } from "@/api/mcpClient";
import { fetchUsers, type OrgUser } from "@/api/users";
import type { Proposal } from "@/api/types";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
import { ProposalsPage } from "./ProposalsPage";

vi.mock("@/api/mcpClient", () => ({
  callMcpTool: vi.fn(),
}));

vi.mock("@/api/users", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/api/users")>();
  return {
    ...actual,
    fetchUsers: vi.fn(),
  };
});

vi.mock("@/components/AuthGate", () => ({
  useAuthUser: () => ({
    id: "user-1",
    github_login: "pm-user",
    github_name: "Pat Manager",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: true,
    role: "pm",
  }),
}));

vi.mock("@/stores/useProjectStore", () => ({
  useProjects: () => [
    { id: "project-1", name: "Core UI", path: "djinnos/djinn" },
  ],
}));

vi.mock("@/components/proposals/useStartProposalChat", () => ({
  useStartProposalChat: () => vi.fn(),
}));

vi.mock("@/lib/toast", () => ({
  showToast: {
    success: vi.fn(),
    error: vi.fn(),
    info: vi.fn(),
  },
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
  {
    id: "user-2",
    github_login: "eng-user",
    github_name: "Eli Engineer",
    github_avatar_url: null,
    is_member_of_org: true,
    is_admin: false,
    role: "engineer",
  },
];

function makeProposal(
  overrides: Partial<Proposal> &
    Pick<Proposal, "id" | "short_id" | "title" | "status">,
): Proposal {
  return {
    id: overrides.id,
    short_id: overrides.short_id,
    title: overrides.title,
    status: overrides.status,
    body: "Proposal body",
    acceptance_criteria: [],
    latest_revision_seq: 1,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-02T00:00:00Z",
    ...overrides,
  };
}

function makeProposalShowResponse(proposal: Proposal) {
  return {
    proposal,
    targets: [],
    feedback: [],
    revisions: [
      {
        id: "rev-1",
        seq: 1,
        title: proposal.title,
        body: "Base body",
        acceptance_criteria: [],
        created_at: "2026-06-01T00:00:00Z",
        edited_by_user_id: "user-1",
      },
      {
        id: "rev-2",
        seq: 2,
        title: proposal.title,
        body: proposal.body,
        acceptance_criteria: proposal.acceptance_criteria,
        created_at: "2026-06-02T00:00:00Z",
        edited_by_user_id: "user-1",
      },
    ],
    signoffs: [],
    epics: [],
  };
}

function mockProposalShow(proposal: Proposal) {
  vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
    if (toolName === "proposal_show") {
      return makeProposalShowResponse(proposal) as never;
    }
    return {} as never;
  });
}

function renderProposalsRoute(initialEntry: string) {
  return render(
    <Routes>
      <Route path="/proposals" element={<ProposalsPage />} />
      <Route path="/proposals/:id" element={<ProposalsPage />} />
    </Routes>,
    { wrapperOptions: { routerProps: { initialEntries: [initialEntry] } } },
  );
}

describe("ProposalsPage", () => {
  beforeEach(() => {
    vi.mocked(callMcpTool).mockReset();
    vi.mocked(fetchUsers).mockReset();
    vi.mocked(fetchUsers).mockResolvedValue(users);
  });

  it("renders proposals grouped by visible lifecycle status", async () => {
    const proposals = [
      makeProposal({
        id: "proposal-approved",
        short_id: "aprv",
        title: "Approve safer rollout",
        status: "approved",
        acceptance_criteria: [{ criterion: "Rollout plan documented", met: true }],
        author_user_id: "user-1",
        updated_at: "2026-06-03T00:00:00Z",
      }),
      makeProposal({
        id: "proposal-review",
        short_id: "revw",
        title: "Review MCP retries",
        status: "in_review",
        unresolved_feedback_count: 2,
        author_user_id: "user-2",
        updated_at: "2026-06-04T00:00:00Z",
      }),
      makeProposal({
        id: "proposal-archived",
        short_id: "arch",
        title: "Archived legacy idea",
        status: "archived",
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    const approvedSection = (await screen.findByText("Approved")).closest("section");
    const reviewSection = screen.getByText("In Review").closest("section");

    expect(approvedSection).not.toBeNull();
    expect(reviewSection).not.toBeNull();
    expect(
      within(approvedSection!).getByText("Approve safer rollout"),
    ).toBeInTheDocument();
    expect(within(approvedSection!).getByText("aprv")).toBeInTheDocument();
    expect(
      within(reviewSection!).getByText("Review MCP retries"),
    ).toBeInTheDocument();
    expect(within(reviewSection!).getByText("revw")).toBeInTheDocument();
    expect(screen.queryByText("Archived legacy idea")).not.toBeInTheDocument();
    expect(callMcpTool).toHaveBeenCalledWith("proposal_list", {
      status: undefined,
      text: undefined,
      target_project: undefined,
      sort: "created_desc",
      limit: 200,
    });
  });

  it("loads proposal detail and sends a status update through proposal_update", async () => {
    const proposal = makeProposal({
      id: "proposal-1",
      short_id: "prop",
      title: "Tighten proposal workflow",
      status: "draft",
      body: "## Why\nOperators need a clearer proposal flow.",
      acceptance_criteria: [{ criterion: "Status changes are persisted", met: false }],
      author_user_id: "user-1",
    });

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_show") {
        return {
          proposal,
          targets: [
            {
              project_id: "project-1",
              project_name: "Core UI",
              project_path: "djinnos/djinn",
              role: "primary",
              created_at: "2026-06-01T00:00:00Z",
            },
          ],
          feedback: [],
          revisions: [
            {
              id: "rev-1",
              seq: 1,
              title: proposal.title,
              body: proposal.body,
              acceptance_criteria: proposal.acceptance_criteria,
              created_at: "2026-06-01T00:00:00Z",
              edited_by_user_id: "user-1",
            },
          ],
          signoffs: [],
          epics: [],
        } as never;
      }
      if (toolName === "proposal_update") {
        return {} as never;
      }
      return {} as never;
    });

    const user = userEvent.setup();
    renderProposalsRoute("/proposals/proposal-1");

    expect(
      await screen.findByRole("heading", { name: "Tighten proposal workflow" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Status changes are persisted")).toBeInTheDocument();
    expect(screen.getByText("Core UI")).toBeInTheDocument();
    expect(
      screen.getByText("Operators need a clearer proposal flow."),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("combobox"));
    await user.click(await screen.findByRole("option", { name: /In Review/i }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith("proposal_update", {
        id: "proposal-1",
        status: "in_review",
      });
    });
  });

  it("hides proposal drift state and diff for non-building proposals", async () => {
    const proposal = makeProposal({
      id: "proposal-done",
      short_id: "done",
      title: "Finished proposal",
      status: "done",
      latest_revision_seq: 2,
      last_reconciled_revision_seq: 1,
      pending_reconcile: true,
    });

    mockProposalShow(proposal);

    renderProposalsRoute("/proposals/proposal-done");

    expect(await screen.findByRole("heading", { name: "Finished proposal" })).toBeInTheDocument();
    expect(screen.queryByText(/spec at rev/)).not.toBeInTheDocument();
    expect(screen.queryByText("Proposal revision diff")).not.toBeInTheDocument();
  });

  it("shows muted reconciled text for building proposals without drift", async () => {
    const proposal = makeProposal({
      id: "proposal-building-clean",
      short_id: "bldc",
      title: "Clean building proposal",
      status: "building",
      latest_revision_seq: 2,
      last_reconciled_revision_seq: 2,
      pending_reconcile: false,
    });

    mockProposalShow(proposal);

    renderProposalsRoute("/proposals/proposal-building-clean");

    expect(await screen.findByText("spec at rev 2 · build reconciled")).toBeInTheDocument();
    expect(screen.getByText("Proposal revision diff")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /reconciling/i })).not.toBeInTheDocument();
  });

  it("shows amber drift text for building proposals pending reconcile", async () => {
    const proposal = makeProposal({
      id: "proposal-building-drift",
      short_id: "bldd",
      title: "Drifting building proposal",
      status: "building",
      latest_revision_seq: 2,
      last_reconciled_revision_seq: 1,
      pending_reconcile: true,
    });

    mockProposalShow(proposal);

    renderProposalsRoute("/proposals/proposal-building-drift");

    expect(
      await screen.findByRole("button", {
        name: "spec at rev 2 · build reconciled to rev 1 · reconciling…",
      }),
    ).toHaveClass("border-amber-500/40");
  });

  it("opens the mounted proposal diff when clicking the drift badge", async () => {
    const proposal = makeProposal({
      id: "proposal-building-open-diff",
      short_id: "bldo",
      title: "Open drift diff",
      status: "building",
      body: "Head body",
      latest_revision_seq: 2,
      last_reconciled_revision_seq: 1,
      pending_reconcile: true,
    });

    mockProposalShow(proposal);

    const user = userEvent.setup();
    renderProposalsRoute("/proposals/proposal-building-open-diff");

    expect(await screen.findByText("Proposal revision diff")).toBeInTheDocument();
    expect(screen.queryByText(/Base body/)).not.toBeInTheDocument();

    await user.click(
      screen.getByRole("button", {
        name: "spec at rev 2 · build reconciled to rev 1 · reconciling…",
      }),
    );

    expect(screen.getByRole("button", { name: "Hide diff" })).toHaveFocus();
    expect(screen.getByText(/Base body/)).toBeInTheDocument();
    expect(screen.getAllByText(/Head body/)).toHaveLength(2);
  });

  it("shows an inline error when proposal_list fails", async () => {
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        throw new Error("Could not load proposals");
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    expect(await screen.findByText("Could not load proposals")).toBeInTheDocument();
  });
});
