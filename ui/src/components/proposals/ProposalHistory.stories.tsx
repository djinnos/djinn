/**
 * Proposals/ProposalHistory — the spec revision timeline. Every material edit
 * appends a full snapshot; a contiguous tribunal run collapses into one
 * "Refined via tribunal" row whose diff is the pre-refinement snapshot → the
 * converged head. Status transitions appear as their own rows. Reads org users
 * via TanStack Query (seeded); expandable rows show a line-level DiffView.
 */

import type { Meta, StoryObj } from "@storybook/react-vite";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { ProposalLintResult } from "@/api/types";
import type { ProposalDetail } from "@/lib/proposalQueries";

import { ProposalHistory } from "./ProposalHistory";
import { richDetail, users } from "./proposalStoryFixtures";

function lint(overrides: Partial<ProposalLintResult> = {}): ProposalLintResult {
  return {
    body_format: "markdown", body_sha256: "storybook", checked_at: "2026-06-02T00:00:00Z",
    errors: [], linter_version: "v1", skipped_tiers: [], warnings: [], ...overrides,
  };
}

function withLint(revisions: Record<number, ProposalLintResult>): ProposalDetail {
  return { ...richDetail, revisions: richDetail.revisions.map((revision) => ({
    ...revision, lint: revisions[revision.seq] ?? revision.lint,
  })) };
}

function makeClient(): QueryClient {
  const qc = new QueryClient({
    defaultOptions: { queries: { retry: false, staleTime: Infinity } },
  });
  qc.setQueryData(["users", "list"], users);
  return qc;
}

const meta = {
  title: "Proposals/ProposalHistory",
  component: ProposalHistory,
  parameters: { layout: "padded" },
  decorators: [
    (Story) => (
      <QueryClientProvider client={makeClient()}>
        <div className="mx-auto max-w-3xl bg-background p-4 text-foreground">
          <Story />
        </div>
      </QueryClientProvider>
    ),
  ],
} satisfies Meta<typeof ProposalHistory>;

export default meta;
type Story = StoryObj<typeof meta>;

/**
 * A seed revision, a two-round tribunal run collapsed into one row, and a
 * draft → in-review status transition. Click a row to expand its diff.
 */
export const WithRevisions: Story = {
  args: { detail: richDetail },
};

export const WarningAndSkippedTier: Story = {
  args: { detail: withLint({ 3: lint({
    warnings: [{ severity: "warning", code: "SPEC_FUTURE_WARNING", message: "An additive server warning", span: { start: 14, end: 28 } }],
    skipped_tiers: [{ tier: "mdx", reason: "LEGACY_BODY_FORMAT", message: "Tier did not apply" }],
  }) }) },
};

export const LegacyErrorAndUnknownCode: Story = {
  args: { detail: withLint({ 3: lint({
    errors: [{ severity: "error", code: "LEGACY_CORRUPT_SPEC", message: "Historical body is corrupt", span: { start: 0, end: 8 } }],
  }) }) },
};

export const OlderWarningThenCleanHead: Story = {
  args: { detail: withLint({
    1: lint({ warnings: [{ severity: "warning", code: "HISTORICAL_WARNING", message: "This warning remains on revision one", span: { start: 1, end: 5 } }] }),
    3: lint(),
  }) },
};
