import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@/test/test-utils";
import { fetchUsers, type OrgUser } from "@/api/users";
import type { Proposal, ProposalLintResult, ProposalRevision } from "@/api/types";
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
    body_format: "markdown",
    acceptance_criteria: [],
    latest_revision_seq: 1,
    pending_reconcile: false,
    created_at: "2026-06-01T00:00:00Z",
    updated_at: "2026-06-02T00:00:00Z",
    ...overrides,
  };
}

function lint(overrides: Partial<ProposalLintResult> = {}): ProposalLintResult {
  return {
    body_format: "markdown",
    body_sha256: "abc123",
    checked_at: "2026-06-02T00:00:00Z",
    errors: [],
    linter_version: "v1",
    skipped_tiers: [],
    warnings: [],
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
    body_format: "markdown",
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
    refinement: null,
    gate_status: null,
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

  it("renders an acceptance-criteria amendment revision with reason and operations", () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1, { body: "Base body" }),
          revision(2, {
            body: "Base body",
            created_at: "2026-06-02T00:00:00Z",
            event_metadata: JSON.stringify({
              kind: "ac_amendment",
              reason: "criterion 2 cannot be verified by agents",
              amendments: [
                {
                  operation: "rewrite",
                  index: 0,
                  old_criterion: "old criterion text",
                  new_criterion: "rewritten criterion text",
                },
                {
                  operation: "drop",
                  index: 1,
                  old_criterion: { criterion: "drop me", met: false },
                  new_criterion: { dropped: true },
                },
                {
                  operation: "waive",
                  index: 6,
                  old_criterion: "waive me",
                  new_criterion: { criterion: "waive me", waived: true },
                },
              ],
            }),
          }),
        ])}
      />,
    );

    // Distinct badge and a machine-free collapsed summary (no raw JSON).
    expect(screen.getByText("AC amendment")).toBeInTheDocument();
    expect(
      screen.getByText(
        "criterion 1 rewritten, criterion 2 dropped, criterion 7 waived",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText(/ac_amendment/)).not.toBeInTheDocument();

    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));

    expect(
      screen.getByText("criterion 2 cannot be verified by agents"),
    ).toBeInTheDocument();
    expect(screen.getByText("Criterion 1 rewritten")).toBeInTheDocument();
    expect(screen.getByText("old criterion text")).toBeInTheDocument();
    expect(screen.getByText("rewritten criterion text")).toBeInTheDocument();
    expect(screen.getByText("Criterion 2 dropped")).toBeInTheDocument();
    expect(screen.getByText("Criterion 7 waived")).toBeInTheDocument();
  });

  it("collapses a tribunal refinement run into a single entry", () => {
    render(
      <ProposalHistory
        detail={detail([
          revision(1, { body: "Original thin spec." }),
          revision(2, {
            body: "Round 1 revision.",
            created_at: "2026-06-02T00:00:00Z",
            event_metadata: JSON.stringify({ source: "refinement_loop", round: 1 }),
          }),
          revision(3, {
            body: "Round 2 converged spec.",
            created_at: "2026-06-03T00:00:00Z",
            event_metadata: JSON.stringify({ source: "refinement_loop", round: 2 }),
          }),
        ])}
      />,
    );

    // One collapsed "Refined via tribunal" entry across 2 rounds, spanning rev 2–3.
    expect(screen.getByText("Refined via tribunal (2 rounds)")).toBeInTheDocument();
    expect(screen.getByText("rev 2–3")).toBeInTheDocument();
    // The intermediate per-round revisions are NOT shown as separate rows.
    expect(screen.queryByText("rev 2")).not.toBeInTheDocument();
  });

  it("keeps an older warning on its own row after a later clean revision", () => {
    render(<ProposalHistory detail={detail([
      revision(1),
      revision(2, { created_at: "2026-06-02T00:00:00Z", lint: lint({ warnings: [{ severity: "warning", code: "SPEC_FUTURE_WARNING", message: "Server supplied warning", span: { start: 3, end: 12 } }] }) }),
      revision(3, { created_at: "2026-06-03T00:00:00Z", lint: lint() }),
    ])} />);

    expect(screen.getByText("1 warning")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /rev 3/ })).not.toHaveTextContent("warning");
    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));
    expect(screen.getByText("SPEC_FUTURE_WARNING")).toBeInTheDocument();
    expect(screen.getByText("Server supplied warning")).toBeInTheDocument();
    expect(screen.getByText("bytes [3, 12)")).toBeInTheDocument();
  });

  it("renders legacy errors, unknown codes, and skipped-tier detail verbatim", () => {
    render(<ProposalHistory detail={detail([
      revision(1),
      revision(2, { created_at: "2026-06-02T00:00:00Z", lint: lint({
        errors: [{ severity: "error", code: "LEGACY_CORRUPT_SPEC", message: "Legacy body is corrupt", span: { start: 0, end: 7 } }],
        skipped_tiers: [{ tier: "mdx", reason: "UNSUPPORTED_LEGACY_FORMAT", message: "MDX parser was not available" }],
      } as ProposalLintResult) }),
    ])} />);

    expect(screen.getByText("1 error")).toBeInTheDocument();
    expect(screen.getByText("1 skipped")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /rev 2/ }));
    expect(screen.getByText("LEGACY_CORRUPT_SPEC")).toBeInTheDocument();
    expect(screen.getByText("Legacy body is corrupt")).toBeInTheDocument();
    expect(screen.getByText("mdx")).toBeInTheDocument();
    expect(screen.getByText("UNSUPPORTED_LEGACY_FORMAT")).toBeInTheDocument();
    expect(screen.getByText("MDX parser was not available")).toBeInTheDocument();
  });

  it("uses only the displayed tribunal head diagnostics", () => {
    render(<ProposalHistory detail={detail([
      revision(1),
      revision(2, { created_at: "2026-06-02T00:00:00Z", event_metadata: JSON.stringify({ source: "refinement_loop", round: 1 }), lint: lint({ errors: [{ severity: "error", code: "HIDDEN_INTERMEDIATE_ERROR", message: "Must not be reattached", span: { start: 0, end: 1 } }] }) }),
      revision(3, { created_at: "2026-06-03T00:00:00Z", event_metadata: JSON.stringify({ source: "refinement_loop", round: 2 }), lint: lint({ warnings: [{ severity: "warning", code: "HEAD_WARNING", message: "Displayed head diagnostic", span: { start: 4, end: 9 } }] }) }),
    ])} />);

    expect(screen.getByText("1 warning")).toBeInTheDocument();
    expect(screen.queryByText("1 error")).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: /rev 2–3/ }));
    expect(screen.getByText("HEAD_WARNING")).toBeInTheDocument();
    expect(screen.queryByText("HIDDEN_INTERMEDIATE_ERROR")).not.toBeInTheDocument();
  });
});
