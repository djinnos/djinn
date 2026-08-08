import { describe, expect, it, vi } from "vitest";
import { fetchUsers } from "@/api/users";
import { render, screen } from "@/test/test-utils";
import type { ProposalFeedbackRefinement } from "@/api/types";
import { FeedbackRefinementCards } from "./FeedbackRefinementCards";

vi.mock("@/api/users", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/api/users")>()),
  fetchUsers: vi.fn().mockResolvedValue([]),
}));
void fetchUsers;

function generation(
  overrides: Partial<ProposalFeedbackRefinement> = {},
): ProposalFeedbackRefinement {
  return {
    root_feedback_id: "root-1",
    generation: 1,
    round: 2,
    state: "queued",
    source_rows: [
      {
        source_feedback_id: "feedback-blocking",
        source_ordinal: 0,
        author_kind: "user",
        author_user_id: "user-1",
        body: "This must be clarified.",
        severity: "blocking",
        created_at: "2026-06-01T00:00:00Z",
      },
      {
        source_feedback_id: "feedback-advisory",
        source_ordinal: 1,
        author_kind: "user",
        author_user_id: "user-2",
        body: "Also consider the empty state.",
        severity: "advisory",
        created_at: "2026-06-01T00:01:00Z",
      },
    ],
    ...overrides,
  };
}

describe("FeedbackRefinementCards", () => {
  it("renders every projected lifecycle state with severity, attribution, and exact links", () => {
    render(
      <FeedbackRefinementCards
        refinements={[
          generation({ state: "queued", debate_entry_id: "debate-queued" }),
          generation({
            generation: 2,
            state: "injected",
            debate_entry_id: "debate-review",
          }),
          generation({ generation: 3, state: "accepted", accepted_revision_seq: 7 }),
          generation({
            generation: 4,
            state: "wont_fix",
            accepted_reason: "The requested behavior conflicts with scope.",
          }),
          generation({ generation: 5, state: "withdrawn_by_author" }),
        ]}
      />,
    );

    expect(screen.getAllByTestId("feedback-refinement-generation")).toHaveLength(5);
    expect(screen.getByText("Queued for refinement")).toBeInTheDocument();
    expect(screen.getByText("Under review")).toBeInTheDocument();
    expect(screen.getByText("Fixed by revision")).toBeInTheDocument();
    expect(screen.getByText("Won't fix")).toBeInTheDocument();
    expect(screen.getByText("Withdrawn by author")).toBeInTheDocument();
    expect(screen.getAllByText("blocking generation")).toHaveLength(5);
    expect(screen.getAllByText("blocking")).toHaveLength(5);
    expect(screen.getAllByText("advisory")).toHaveLength(5);
    expect(screen.getAllByText("reviewer")).toHaveLength(10);
    expect(screen.getByText("The requested behavior conflicts with scope.")).toBeInTheDocument();
    expect(screen.getAllByRole("link", { name: "View source debate entry" })[0]).toHaveAttribute(
      "href",
      "#proposal-debate-entry-debate-queued",
    );
    expect(screen.getByRole("link", { name: "View accepted revision 7" })).toHaveAttribute(
      "href",
      "#proposal-revision-7",
    );
  });

  it("keeps advisory-only generations visible without a readiness-blocking label", () => {
    render(
      <FeedbackRefinementCards
        refinements={[
          generation({
            state: "queued",
            source_rows: [
              {
                source_feedback_id: "feedback-advisory-only",
                source_ordinal: 0,
                author_kind: "user",
                body: "A non-gating suggestion.",
                severity: "advisory",
                created_at: "2026-06-01T00:00:00Z",
              },
            ],
          }),
        ]}
      />,
    );

    expect(screen.getAllByText("advisory")).toHaveLength(2);
    expect(screen.queryByText("blocking generation")).not.toBeInTheDocument();
  });
});
