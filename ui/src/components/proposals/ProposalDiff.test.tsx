import { createRef } from "react";
import { describe, expect, it, vi } from "vitest";
import { act, render, screen, userEvent, within } from "@/test/test-utils";
import type { ProposalRevision } from "@/api/types";
import { ProposalDiff, type ProposalDiffHandle } from "./ProposalDiff";

function revision(
  seq: number,
  overrides: Partial<ProposalRevision> = {},
): ProposalRevision {
  return {
    id: `rev-${seq}`,
    seq,
    title: "Original proposal",
    body: "Keep the existing body.",
    acceptance_criteria: [{ criterion: "Existing criterion", met: false }],
    event_kind: "spec_revision",
    created_at: "2026-06-01T00:00:00Z",
    ...overrides,
  };
}

describe("ProposalDiff", () => {
  it("is default-closed and renders title, body, and acceptance criteria changes", async () => {
    const user = userEvent.setup();
    render(
      <ProposalDiff
        revisions={[
          revision(1, {
            title: "Original proposal",
            body: "Keep the existing body.",
            acceptance_criteria: [{ criterion: "Existing criterion", met: false }],
          }),
          revision(2, {
            title: "Updated proposal",
            body: "Keep the existing body.\nAdd a safer rollout plan.",
            acceptance_criteria: [
              { criterion: "Existing criterion", met: true },
              "New rollback criterion",
            ],
          }),
        ]}
        baseSeq={1}
        headSeq={2}
      />,
    );

    expect(screen.queryByText("# Updated proposal")).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Show diff" }));

    const panel = screen.getByText("# Updated proposal").closest("pre");
    expect(panel).not.toBeNull();
    expect(within(panel!).getByText("# Original proposal")).toBeInTheDocument();
    expect(within(panel!).getByText("# Updated proposal")).toBeInTheDocument();
    expect(
      within(panel!).getByText("Add a safer rollout plan."),
    ).toBeInTheDocument();
    expect(within(panel!).getByText("- [ ] Existing criterion")).toBeInTheDocument();
    expect(within(panel!).getByText("- [x] Existing criterion")).toBeInTheDocument();
    expect(within(panel!).getByText("- New rollback criterion")).toBeInTheDocument();
  });

  it("renders a no-change state", async () => {
    const user = userEvent.setup();
    const unchanged = {
      title: "Original proposal",
      body: "Keep the existing body.",
      acceptance_criteria: [{ criterion: "Existing criterion", met: false }],
    } satisfies Partial<ProposalRevision>;
    render(
      <ProposalDiff
        revisions={[revision(1, unchanged), revision(2, unchanged)]}
        baseSeq={1}
        headSeq={2}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Show diff" }));

    expect(screen.getByText("No textual changes.")).toBeInTheDocument();
  });

  it("renders MDX block tags as escaped diff text", async () => {
    const user = userEvent.setup();
    const { container } = render(
      <ProposalDiff
        revisions={[
          revision(1, {
            body_format: "mdx",
            body: [
              "Plan the data contract.",
              '<data-model id="users">',
              "User { id: uuid }",
              "</data-model>",
              '<question-form id="open-questions">',
              "- Should user IDs be public?",
              "</question-form>",
            ].join("\n"),
          }),
          revision(2, {
            body_format: "mdx",
            body: [
              "Plan the data contract.",
              '<data-model id="users">',
              "User { id: uuid, email: string }",
              "</data-model>",
              '<question-form id="open-questions">',
              "- Should user IDs be public?",
              "- Who owns PII deletion?",
              "</question-form>",
            ].join("\n"),
          }),
        ]}
        baseSeq={1}
        headSeq={2}
      />,
    );

    await user.click(screen.getByRole("button", { name: "Show diff" }));

    expect(screen.getByText("## Body (mdx)")).toBeInTheDocument();
    expect(screen.getByText('<data-model id="users">')).toBeInTheDocument();
    expect(screen.getByText("User { id: uuid }")).toBeInTheDocument();
    expect(
      screen.getByText("User { id: uuid, email: string }"),
    ).toBeInTheDocument();
    expect(screen.getByText("- Who owns PII deletion?")).toBeInTheDocument();
    expect(container.querySelector("data-model")).toBeNull();
    expect(container.querySelector("question-form")).toBeNull();
  });

  it("renders a graceful missing base revision state", async () => {
    const user = userEvent.setup();
    render(<ProposalDiff revisions={[revision(2)]} baseSeq={1} headSeq={2} />);

    await user.click(screen.getByRole("button", { name: "Show diff" }));

    expect(
      screen.getByText(
        "Base revision 1 is not available. The diff cannot be displayed.",
      ),
    ).toBeInTheDocument();
  });

  it("can be opened by a controlled parent", () => {
    const onOpenChange = vi.fn();
    render(
      <ProposalDiff
        revisions={[revision(1), revision(2, { body: "Changed body." })]}
        baseSeq={1}
        headSeq={2}
        open
        onOpenChange={onOpenChange}
      />,
    );

    expect(screen.getByText("Changed body.")).toBeInTheDocument();
    screen.getByRole("button", { name: "Hide diff" }).click();
    expect(onOpenChange).toHaveBeenCalledWith(false);
  });

  it("can be opened through an imperative ref", () => {
    const ref = createRef<ProposalDiffHandle>();
    render(
      <ProposalDiff
        ref={ref}
        revisions={[revision(1), revision(2, { body: "Changed by ref." })]}
        baseSeq={1}
        headSeq={2}
      />,
    );

    expect(screen.queryByText("Changed by ref.")).not.toBeInTheDocument();
    act(() => {
      ref.current?.open();
    });
    expect(screen.getByText("Changed by ref.")).toBeInTheDocument();
  });
});
