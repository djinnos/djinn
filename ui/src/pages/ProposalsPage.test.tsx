import { beforeEach, describe, expect, it, vi } from "vitest";
import { Route, Routes } from "react-router-dom";
import { callMcpTool } from "@/api/mcpClient";
import { fetchUsers, type OrgUser } from "@/api/users";
import type {
  Proposal,
  ProposalEpic,
  ProposalLintResult,
  ProposalListRow,
} from "@/api/types";
import { render, screen, userEvent, waitFor, within } from "@/test/test-utils";
import { ProposalsPage } from "./ProposalsPage";

const scrollIntoViewMock = vi.fn();

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
    body_format: "markdown",
    acceptance_criteria: [],
    latest_revision_seq: 1,
    pending_reconcile: false,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-02T00:00:00Z",
    ...overrides,
  };
}

function makeLint(overrides: Partial<ProposalLintResult> = {}): ProposalLintResult {
  return {
    body_format: "markdown",
    body_sha256: "lint-fixture-sha",
    checked_at: "2026-06-02T00:00:00Z",
    errors: [],
    linter_version: "v1",
    skipped_tiers: [],
    warnings: [],
    ...overrides,
  };
}

/**
 * Build a lean `ProposalListRow` fixture — the bounded summary shape returned
 * by `proposal_list` (no body, criteria, or detail bookkeeping). List test
 * fixtures use this; detail tests keep `makeProposal` for `proposal_show`.
 */
function makeListRow(
  overrides: Partial<ProposalListRow> &
    Pick<ProposalListRow, "id" | "short_id" | "title" | "status">,
): ProposalListRow {
  return {
    id: overrides.id,
    short_id: overrides.short_id,
    title: overrides.title,
    status: overrides.status,
    pending_reconcile: false,
    ac_total: 0,
    ac_met: 0,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-02T00:00:00Z",
    ...overrides,
  };
}

function makeProposalShowResponse(
  proposal: Proposal,
  overrides: { epics?: ProposalEpic[]; latest_lint?: ProposalLintResult | null } = {},
) {
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
        event_kind: "spec_revision",
        created_at: "2026-06-01T00:00:00Z",
        edited_by_user_id: "user-1",
      },
      {
        id: "rev-2",
        seq: 2,
        title: proposal.title,
        body: proposal.body,
        acceptance_criteria: proposal.acceptance_criteria,
        event_kind: "spec_revision",
        created_at: "2026-06-02T00:00:00Z",
        edited_by_user_id: "user-1",
      },
    ],
    signoffs: [],
    epics: [],
    ...overrides,
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
    scrollIntoViewMock.mockReset();
    Element.prototype.scrollIntoView = scrollIntoViewMock;
    window.history.pushState({}, "", "/");
  });

  it("scrolls to and temporarily highlights an MDX block from the block query param", async () => {
    let removeHighlight: (() => void) | undefined;
    // Capture ONLY the 3s highlight-removal timer; everything else must reach
    // the real setTimeout — testing-library's waitFor/findBy* polls through it,
    // and swallowing those timers hangs this test AND (because the vitest
    // timeout abandons the promise before `finally` restores the spy) leaves
    // window.setTimeout broken for every test after it.
    const realSetTimeout = window.setTimeout.bind(window);
    const setTimeoutSpy = vi
      .spyOn(window, "setTimeout")
      .mockImplementation(((
        handler: TimerHandler,
        timeout?: number,
        ...args: unknown[]
      ) => {
        if (timeout === 3000 && typeof handler === "function") {
          removeHighlight = handler as () => void;
          return 1;
        }
        return realSetTimeout(handler, timeout, ...args);
      }) as typeof window.setTimeout);
    const proposal = makeProposal({
      id: "proposal-anchor",
      short_id: "anch",
      title: "Anchored proposal",
      status: "draft",
      body_format: "mdx",
      body: '<RichText id="target-block">Anchored block content</RichText>',
    });

    mockProposalShow(proposal);

    try {
      renderProposalsRoute("/proposals/proposal-anchor?block=target-block");

      expect(await screen.findByText("Anchored block content")).toBeInTheDocument();

      const target = document.getElementById("target-block");
      expect(target).not.toBeNull();
      expect(scrollIntoViewMock).toHaveBeenCalledWith({
        behavior: "smooth",
        block: "center",
      });
      expect(target).toHaveClass("ring-2", "ring-primary", "duration-1000");
      expect(setTimeoutSpy).toHaveBeenCalledWith(expect.any(Function), 3000);

      removeHighlight?.();

      expect(target).not.toHaveClass("ring-2", "ring-primary");
    } finally {
      setTimeoutSpy.mockRestore();
    }
  });

  it("treats a block query param on markdown proposals as a no-op", async () => {
    const proposal = makeProposal({
      id: "proposal-markdown-anchor",
      short_id: "mkan",
      title: "Markdown anchored proposal",
      status: "draft",
      body_format: "markdown",
      body: "## Plain markdown\n\nNo block ids here.",
    });

    mockProposalShow(proposal);

    renderProposalsRoute("/proposals/proposal-markdown-anchor?block=missing-block");

    expect(await screen.findByText("No block ids here.")).toBeInTheDocument();
    expect(scrollIntoViewMock).not.toHaveBeenCalled();
  });

  it("renders proposals grouped by visible lifecycle status", async () => {
    const proposals = [
      makeListRow({
        id: "proposal-approved",
        short_id: "aprv",
        title: "Approve safer rollout",
        status: "approved",
        ac_total: 1,
        ac_met: 1,
        author_user_id: "user-1",
        updated_at: "2026-06-03T00:00:00Z",
      }),
      makeListRow({
        id: "proposal-review",
        short_id: "revw",
        title: "Review MCP retries",
        status: "in_review",
        unresolved_feedback_count: 2,
        author_user_id: "user-2",
        updated_at: "2026-06-04T00:00:00Z",
      }),
      makeListRow({
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

    const approvedSection = (await screen.findByText("Approved")).closest(
      "section",
    );
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
    const reviewRow = within(reviewSection!)
      .getByText("Review MCP retries")
      .closest("button");
    expect(reviewRow).not.toBeNull();
    const unresolvedFeedback = within(reviewRow!).getByTitle(
      "2 unresolved comments",
    );
    expect(unresolvedFeedback).toBeVisible();
    expect(unresolvedFeedback).toHaveAttribute(
      "title",
      "2 unresolved comments",
    );
    expect(unresolvedFeedback).toHaveTextContent("2");
    expect(screen.queryByText("Archived legacy idea")).not.toBeInTheDocument();
    // List query sends no verbosity flags.
    expect(callMcpTool).toHaveBeenCalledWith("proposal_list", {
      status: undefined,
      text: undefined,
      target_project: undefined,
      sort: "created_desc",
      limit: 200,
    });
  });

  it("renders one unified tribunal chip and a gate dot from each row's list summary", async () => {
    const proposals = [
      makeListRow({
        id: "p-review",
        short_id: "rvw1",
        title: "Awaiting review proposal",
        status: "in_review",
        list_summary: {
          refinement_active: true,
          awaiting_review: true,
          current_round: 4,
          needs_evidence: false,
          dor_ready: true,
          gate_ready: true,
          unresolved_blocking_count: 0,
        },
      }),
      makeListRow({
        id: "p-running",
        short_id: "rvw2",
        title: "Refinement running proposal",
        status: "in_review",
        list_summary: {
          refinement_active: true,
          awaiting_review: false,
          current_round: 3,
          needs_evidence: false,
          dor_ready: false,
          gate_ready: false,
          unresolved_blocking_count: 0,
        },
      }),
      makeListRow({
        id: "p-evidence",
        short_id: "rvw3",
        title: "Evidence parked proposal",
        status: "in_review",
        list_summary: {
          refinement_active: true,
          awaiting_review: false,
          current_round: 2,
          needs_evidence: true,
          dor_ready: true,
          gate_ready: false,
          unresolved_blocking_count: 0,
        },
      }),
      makeListRow({
        id: "p-blocked",
        short_id: "rvw4",
        title: "Blocked by objections proposal",
        status: "in_review",
        list_summary: {
          refinement_active: false,
          awaiting_review: false,
          current_round: 1,
          needs_evidence: false,
          dor_ready: true,
          gate_ready: false,
          unresolved_blocking_count: 2,
        },
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    const reviewSection = (await screen.findByText("In Review")).closest(
      "section",
    );
    expect(reviewSection).not.toBeNull();
    const section = within(reviewSection!);

    // One tribunal icon per state, identified by its tooltip — the icon-only
    // treatment keeps the row text-free.
    expect(
      section.getByTitle("Tribunal converged — awaiting your review"),
    ).toBeInTheDocument();
    expect(
      section.getByTitle("Tribunal refinement running (round 3)"),
    ).toBeInTheDocument();
    expect(
      section.getByTitle("Parked on a needs-evidence spike"),
    ).toBeInTheDocument();

    // Idle-but-blocking row shows the red scale with just the open count.
    expect(
      section.getByTitle(
        "Tribunal round 1 left 2 unresolved blocking objections",
      ),
    ).toHaveTextContent("2");

    // Gate icons carry their reason in the tooltip only, and a READY gate
    // renders nothing at all.
    expect(section.queryByTitle("Gate ready")).not.toBeInTheDocument();
    expect(
      section.getByTitle("Gate blocked — 2 unresolved blocking objections"),
    ).toBeInTheDocument();
    expect(
      section.getByTitle("Gate blocked — parked on evidence"),
    ).toBeInTheDocument();
  });

  it("shows a neutral (not red) gate for a fresh draft failing Definition of Ready", async () => {
    const proposals = [
      makeListRow({
        id: "p-fresh-draft",
        short_id: "frsh",
        title: "Fresh draft proposal",
        status: "draft",
        // No tribunal has ever engaged — DoR failing is the expected state.
        list_summary: {
          refinement_active: false,
          awaiting_review: false,
          current_round: 0,
          needs_evidence: false,
          dor_ready: false,
          gate_ready: false,
          unresolved_blocking_count: 0,
        },
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    // Faint neutral shield, explanatory tooltip, no red alarm.
    const gate = await screen.findByTitle("Definition of Ready not yet met");
    expect(gate).toBeInTheDocument();
    expect(gate).toHaveClass("text-muted-foreground/50");
    expect(gate).not.toHaveClass("text-red-500");
    expect(screen.queryByText(/Gate blocked/)).not.toBeInTheDocument();
  });

  it("shows a red gate only once a tribunal has engaged and the gate is stuck", async () => {
    const proposals = [
      makeListRow({
        id: "p-engaged-blocked",
        short_id: "engb",
        title: "Engaged and blocked proposal",
        status: "in_review",
        list_summary: {
          refinement_active: false,
          awaiting_review: false,
          current_round: 2,
          needs_evidence: false,
          dor_ready: true,
          gate_ready: false,
          unresolved_blocking_count: 3,
        },
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    const gate = await screen.findByTitle(
      "Gate blocked — 3 unresolved blocking objections",
    );
    expect(gate).toHaveClass("text-red-500");
  });

  it("sorts awaiting-review rows first and summarizes them in the group header", async () => {
    const proposals = [
      makeListRow({
        id: "p-recent-plain",
        short_id: "rp01",
        title: "Recently updated plain row",
        status: "in_review",
        updated_at: "2026-06-10T00:00:00Z",
        list_summary: {
          refinement_active: true,
          awaiting_review: false,
          current_round: 1,
          needs_evidence: false,
          dor_ready: false,
          gate_ready: false,
          unresolved_blocking_count: 0,
        },
      }),
      makeListRow({
        id: "p-older-awaiting",
        short_id: "oa01",
        title: "Older awaiting review row",
        status: "in_review",
        updated_at: "2026-06-01T00:00:00Z",
        list_summary: {
          refinement_active: false,
          awaiting_review: true,
          current_round: 2,
          needs_evidence: false,
          dor_ready: true,
          gate_ready: true,
          unresolved_blocking_count: 0,
        },
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    // Header summarizes the awaiting-review count.
    expect(
      await screen.findByRole("button", {
        name: /In Review\s+2\s+·\s+1 awaiting review/,
      }),
    ).toBeInTheDocument();

    // The older awaiting-review row is ordered before the more-recent plain one.
    const titles = screen
      .getAllByText(/review row|plain row/)
      .map((el) => el.textContent);
    expect(titles).toEqual([
      "Older awaiting review row",
      "Recently updated plain row",
    ]);
  });

  it("renders acceptance progress from ac_met/ac_total and hides badge when total is zero", async () => {
    const proposals = [
      makeListRow({
        id: "p-progress",
        short_id: "prog",
        title: "Progress proposal",
        status: "in_review",
        ac_total: 3,
        ac_met: 2,
      }),
      makeListRow({
        id: "p-zero",
        short_id: "zero",
        title: "Zero criteria proposal",
        status: "in_review",
        ac_total: 0,
        ac_met: 0,
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    // The progress badge renders 2/3 for the proposal with criteria.
    const reviewSection = (await screen.findByText("In Review")).closest(
      "section",
    );
    expect(reviewSection).not.toBeNull();
    expect(within(reviewSection!).getByText("2/3")).toBeInTheDocument();
    // No badge text like 0/0 should appear for the zero-criteria row.
    expect(within(reviewSection!).queryByText("0/0")).not.toBeInTheDocument();
  });

  it("defaults terminal proposal groups collapsed and active groups expanded with counts visible", async () => {
    const proposals = [
      makeListRow({
        id: "proposal-done",
        short_id: "done",
        title: "Completed proposal",
        status: "done",
      }),
      makeListRow({
        id: "proposal-building",
        short_id: "bldg",
        title: "Building proposal",
        status: "building",
      }),
      makeListRow({
        id: "proposal-rejected",
        short_id: "rjct",
        title: "Rejected proposal",
        status: "rejected",
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    const doneHeader = await screen.findByRole("button", { name: /Done\s+1/ });
    const buildingHeader = screen.getByRole("button", { name: /Building\s+1/ });
    const rejectedHeader = screen.getByRole("button", { name: /Rejected\s+1/ });

    expect(doneHeader).toHaveAttribute("aria-expanded", "false");
    expect(buildingHeader).toHaveAttribute("aria-expanded", "true");
    expect(rejectedHeader).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("Completed proposal")).not.toBeInTheDocument();
    expect(screen.getByText("Building proposal")).toBeInTheDocument();
    expect(screen.queryByText("Rejected proposal")).not.toBeInTheDocument();
  });

  it("expands a terminal row without a list summary and renders no tribunal chip or gate dot", async () => {
    const proposals = [
      makeListRow({
        id: "proposal-done",
        short_id: "done",
        title: "Completed proposal",
        status: "done",
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    const user = userEvent.setup();
    renderProposalsRoute("/proposals");

    const doneHeader = await screen.findByRole("button", { name: /Done\s+1/ });

    expect(doneHeader).toHaveAttribute("aria-expanded", "false");
    expect(within(doneHeader).getByText("1")).toBeInTheDocument();
    expect(screen.queryByText("Completed proposal")).not.toBeInTheDocument();

    await user.click(doneHeader);

    expect(doneHeader).toHaveAttribute("aria-expanded", "true");
    expect(within(doneHeader).getByText("1")).toBeInTheDocument();
    const terminalRow = screen.getByText("Completed proposal").closest("button");
    expect(terminalRow).not.toBeNull();
    expect(
      within(terminalRow!).queryByTitle(
        /^(Tribunal|Parked on a needs-evidence spike)/,
      ),
    ).not.toBeInTheDocument();
    expect(
      within(terminalRow!).queryByTitle(/^(Gate |Definition of Ready)/),
    ).not.toBeInTheDocument();
  });

  it("keeps archived and superseded proposal groups hidden until Show archived is enabled", async () => {
    const proposals = [
      makeListRow({
        id: "proposal-superseded",
        short_id: "supr",
        title: "Superseded proposal",
        status: "superseded",
      }),
      makeListRow({
        id: "proposal-archived",
        short_id: "arch",
        title: "Archived proposal",
        status: "archived",
      }),
    ];

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        return { proposals } as never;
      }
      return {} as never;
    });

    const user = userEvent.setup();
    renderProposalsRoute("/proposals");

    expect(await screen.findByText("No proposals yet.")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Superseded\s+1/ }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /Archived\s+1/ }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByLabelText("Show archived"));

    expect(
      await screen.findByRole("button", { name: /Superseded\s+1/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Archived\s+1/ })).toBeInTheDocument();
  });

  it("loads proposal detail and sends a status update through proposal_update", async () => {
    const proposal = makeProposal({
      id: "proposal-1",
      short_id: "prop",
      title: "Tighten proposal workflow",
      status: "draft",
      body: "## Why\nOperators need a clearer proposal flow.",
      acceptance_criteria: [
        { criterion: "Status changes are persisted", met: false },
      ],
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
              event_kind: "spec_revision",
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
    expect(
      screen.getByText("Status changes are persisted"),
    ).toBeInTheDocument();
    expect(screen.getByText("Core UI")).toBeInTheDocument();
    expect(
      screen.getByText("Operators need a clearer proposal flow."),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("combobox", { name: "Proposal status" }));
    await user.click(await screen.findByRole("option", { name: /In Review/i }));

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith("proposal_update", {
        id: "proposal-1",
        status: "in_review",
      });
    });
  });

  it("offers a clear manual done action for non-terminal proposals through proposal_update", async () => {
    const proposal = makeProposal({
      id: "proposal-1",
      short_id: "prop",
      title: "Externally implemented proposal",
      status: "draft",
      author_user_id: "user-1",
    });

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_show") {
        return makeProposalShowResponse(proposal) as never;
      }
      if (toolName === "proposal_update") {
        return {} as never;
      }
      return {} as never;
    });

    const user = userEvent.setup();
    renderProposalsRoute("/proposals/proposal-1");

    expect(
      await screen.findByRole("heading", { name: "Externally implemented proposal" }),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("combobox", { name: "Proposal status" }));
    await user.click(
      await screen.findByRole("option", {
        name: /Mark done \(implemented externally\)/i,
      }),
    );

    await waitFor(() => {
      expect(callMcpTool).toHaveBeenCalledWith("proposal_update", {
        id: "proposal-1",
        status: "done",
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

    expect(
      await screen.findByRole("heading", { name: "Finished proposal" }),
    ).toBeInTheDocument();
    expect(screen.queryByText(/spec at rev/)).not.toBeInTheDocument();
    expect(
      screen.queryByText("Proposal revision diff"),
    ).not.toBeInTheDocument();
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

    expect(
      await screen.findByText("spec at rev 2 · build reconciled"),
    ).toBeInTheDocument();
    expect(screen.getByText("Proposal revision diff")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: /reconciling/i }),
    ).not.toBeInTheDocument();
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

    expect(
      await screen.findByText("Proposal revision diff"),
    ).toBeInTheDocument();
    // The history timeline previews revision bodies, so "Base body" may
    // legitimately be on the page while the diff is closed — assert on the
    // diff's own chrome and on the COUNT of body occurrences instead.
    expect(
      screen.queryByRole("button", { name: "Hide diff" }),
    ).not.toBeInTheDocument();
    const baseBodyBefore = screen.queryAllByText(/Base body/).length;
    const headBodyBefore = screen.queryAllByText(/Head body/).length;

    await user.click(
      screen.getByRole("button", {
        name: "spec at rev 2 · build reconciled to rev 1 · reconciling…",
      }),
    );

    expect(screen.getByRole("button", { name: "Hide diff" })).toHaveFocus();
    expect(screen.getAllByText(/Base body/)).toHaveLength(baseBodyBefore + 1);
    expect(screen.getAllByText(/Head body/)).toHaveLength(headBodyBefore + 1);
  });

  it("shows an amber needs-reconcile badge for graduated epics that need reconcile", async () => {
    const proposal = makeProposal({
      id: "proposal-epic-needs-reconcile",
      short_id: "epnr",
      title: "Epic needs reconcile",
      status: "building",
    });

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_show") {
        return makeProposalShowResponse(proposal, {
          epics: [
            {
              epic_id: "epic-needs-reconcile",
              epic_short_id: "need",
              epic_title: "Refresh spawned epic",
              epic_emoji: "🛠️",
              project_path: "djinnos/djinn",
              status: "open",
              reconciled_at_revision_seq: 1,
              needs_reconcile: true,
            },
          ],
        }) as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals/proposal-epic-needs-reconcile");

    expect(await screen.findByText("Refresh spawned epic")).toBeInTheDocument();
    expect(screen.getByText("needs reconcile")).toHaveClass(
      "border-amber-500/40",
    );
  });

  it("shows a muted reconciled-at badge for graduated epics with reconcile history", async () => {
    const proposal = makeProposal({
      id: "proposal-epic-reconciled",
      short_id: "eprc",
      title: "Epic reconciled",
      status: "building",
    });

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_show") {
        return makeProposalShowResponse(proposal, {
          epics: [
            {
              epic_id: "epic-reconciled",
              epic_short_id: "reco",
              epic_title: "Already reconciled epic",
              epic_emoji: "✅",
              project_path: "djinnos/djinn",
              status: "open",
              reconciled_at_revision_seq: 3,
              needs_reconcile: false,
            },
          ],
        }) as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals/proposal-epic-reconciled");

    expect(
      await screen.findByText("Already reconciled epic"),
    ).toBeInTheDocument();
    expect(screen.getByText("reconciled at rev 3")).toHaveClass(
      "text-muted-foreground",
    );
    expect(screen.queryByText("needs reconcile")).not.toBeInTheDocument();
  });

  it("hides reconcile badges for graduated epics with no reconcile history", async () => {
    const proposal = makeProposal({
      id: "proposal-epic-no-history",
      short_id: "epnh",
      title: "Epic no reconcile history",
      status: "building",
    });

    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_show") {
        return makeProposalShowResponse(proposal, {
          epics: [
            {
              epic_id: "epic-no-history",
              epic_short_id: "hist",
              epic_title: "Pre-amend spawned epic",
              epic_emoji: "📌",
              project_path: "djinnos/djinn",
              status: "open",
              reconciled_at_revision_seq: null,
              needs_reconcile: false,
            },
          ],
        }) as never;
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals/proposal-epic-no-history");

    expect(
      await screen.findByText("Pre-amend spawned epic"),
    ).toBeInTheDocument();
    expect(screen.queryByText("needs reconcile")).not.toBeInTheDocument();
    expect(screen.queryByText(/reconciled at rev/)).not.toBeInTheDocument();
  });

  it("shows amber latest-head integrity diagnostics for warnings", async () => {
    const proposal = makeProposal({ id: "lint-warning", short_id: "lwrn", title: "Warning head", status: "draft" });
    vi.mocked(callMcpTool).mockImplementation(async (toolName) =>
      toolName === "proposal_show"
        ? (makeProposalShowResponse(proposal, { latest_lint: makeLint({ warnings: [{ severity: "warning", code: "SPEC_STYLE_FUTURE", message: "Server warning", span: { start: 4, end: 12 } }] }) }) as never)
        : ({} as never),
    );
    renderProposalsRoute("/proposals/lint-warning");
    const banner = await screen.findByRole("alert", { name: "Latest proposal integrity" });
    expect(banner).toHaveTextContent("Proposal integrity warnings");
    expect(banner).toHaveTextContent("warning · SPEC_STYLE_FUTURE · Server warning bytes [4, 12)");
    expect(banner).toHaveClass("border-amber-200");
  });

  it("shows destructive legacy errors and gives them precedence over warnings", async () => {
    const proposal = makeProposal({ id: "lint-mixed", short_id: "lmix", title: "Legacy error head", status: "draft" });
    vi.mocked(callMcpTool).mockImplementation(async (toolName) =>
      toolName === "proposal_show"
        ? (makeProposalShowResponse(proposal, { latest_lint: makeLint({
            errors: [{ severity: "error", code: "SPEC_LEGACY_CORRUPT", message: "Legacy body is corrupt", span: { start: 0, end: 7 } }],
            warnings: [{ severity: "warning", code: "SPEC_WARNING", message: "Warning detail", span: { start: 8, end: 11 } }],
          }) }) as never)
        : ({} as never),
    );
    renderProposalsRoute("/proposals/lint-mixed");
    const banner = await screen.findByRole("alert", { name: "Latest proposal integrity" });
    expect(banner).toHaveTextContent("Proposal integrity errors");
    expect(banner).not.toHaveTextContent("Proposal integrity warnings");
    expect(banner).toHaveTextContent("error · SPEC_LEGACY_CORRUPT · Legacy body is corrupt bytes [0, 7)");
    expect(banner).toHaveTextContent("Warning detail");
    expect(banner).toHaveClass("border-destructive/40");
  });

  it("renders unknown server codes generically", async () => {
    const proposal = makeProposal({ id: "lint-unknown", short_id: "lunk", title: "Unknown lint head", status: "draft" });
    vi.mocked(callMcpTool).mockImplementation(async (toolName) =>
      toolName === "proposal_show"
        ? (makeProposalShowResponse(proposal, { latest_lint: makeLint({ warnings: [{ severity: "warning", code: "FUTURE_OPAQUE_42", message: "Future server diagnostic", span: { start: 101, end: 144 } }] }) }) as never)
        : ({} as never),
    );
    renderProposalsRoute("/proposals/lint-unknown");
    expect(await screen.findByText("FUTURE_OPAQUE_42")).toBeInTheDocument();
    expect(screen.getByText("bytes [101, 144)")).toBeInTheDocument();
  });

  it("omits integrity banners for clean, null, and absent lint data", async () => {
    for (const [kind, latest_lint] of [["clean", makeLint()], ["null", null], ["absent", undefined]] as const) {
      const proposal = makeProposal({ id: `lint-${kind}`, short_id: kind, title: `${kind} lint head`, status: "draft" });
      vi.mocked(callMcpTool).mockImplementation(async (toolName) =>
        toolName === "proposal_show"
          ? (makeProposalShowResponse(proposal, latest_lint === undefined ? {} : { latest_lint }) as never)
          : ({} as never),
      );
      const view = renderProposalsRoute(`/proposals/lint-${kind}`);
      await screen.findByRole("heading", { name: `${kind} lint head` });
      expect(screen.queryByRole("alert", { name: "Latest proposal integrity" })).not.toBeInTheDocument();
      view.unmount();
    }
  });

  it("shows an inline error when proposal_list fails", async () => {
    vi.mocked(callMcpTool).mockImplementation(async (toolName) => {
      if (toolName === "proposal_list") {
        throw new Error("Could not load proposals");
      }
      return {} as never;
    });

    renderProposalsRoute("/proposals");

    expect(
      await screen.findByText("Could not load proposals"),
    ).toBeInTheDocument();
  });
});
