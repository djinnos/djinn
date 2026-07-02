import { describe, expect, it } from "vitest";

import type {
  UsageAnalyticsResponse,
  UsageBreakdownRow,
} from "@/api/analytics";
import { render, screen } from "@/test/test-utils";
import { UsageBreakdownsTab } from "./UsageBreakdownsTab";

function makeBreakdownRow(
  overrides: Partial<UsageBreakdownRow> = {},
): UsageBreakdownRow {
  return {
    id: "user-1",
    name: "Alice",
    cost: 15.0,
    tokens_in: 5000,
    tokens_out: 2500,
    task_count: 4,
    success_rate: 0.8,
    avg_reopens: 0.5,
    cost_per_task: 3.75,
    actual_spend_usd: 10.0,
    projected_usd: 5.0,
    unpriced_count: 2,
    ...overrides,
  };
}

function makeResponse(
  rows: UsageBreakdownRow[],
): UsageAnalyticsResponse {
  return {
    kpis: [],
    time_series: [],
    breakdowns: {
      by_user: rows,
      by_project: [],
      by_proposal: [],
      by_task: [],
    },
    model_effectiveness: [],
    project_model_matrix: [],
    generated_at: "2026-06-25T12:00:00Z",
  };
}

describe("UsageBreakdownsTab — split cost column labels", () => {
  it("renders Actual API spend and Projected subscription-equivalent cost column headers", () => {
    render(<UsageBreakdownsTab data={makeResponse([makeBreakdownRow()])} />);

    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
    expect(screen.getByText("Unpriced")).toBeInTheDocument();
  });

  it("renders actual and projected values from split fields", () => {
    render(
      <UsageBreakdownsTab
        data={makeResponse([
          makeBreakdownRow({
            actual_spend_usd: 25.0,
            projected_usd: 12.5,
            unpriced_count: 3,
          }),
        ])}
      />,
    );

    expect(screen.getByText("$25.00")).toBeInTheDocument();
    expect(screen.getByText("$12.50")).toBeInTheDocument();
    expect(screen.getByText("3")).toBeInTheDocument();
  });

  it("renders em dash for null split fields instead of $0", () => {
    render(
      <UsageBreakdownsTab
        data={makeResponse([
          makeBreakdownRow({
            actual_spend_usd: null,
            projected_usd: null,
            unpriced_count: 0,
          }),
        ])}
      />,
    );

    // Both cost cells should show em dash
    const dashes = screen.getAllByText("—");
    expect(dashes.length).toBeGreaterThanOrEqual(2);
  });

  it("renders unpriced count as integer, not as $0", () => {
    render(
      <UsageBreakdownsTab
        data={makeResponse([
          makeBreakdownRow({
            actual_spend_usd: null,
            projected_usd: null,
            unpriced_count: 5,
          }),
        ])}
      />,
    );

    expect(screen.getByText("5")).toBeInTheDocument();
    // Should not have $0 anywhere for unpriced
    expect(screen.queryByText("$0.00")).not.toBeInTheDocument();
  });

  it("shows methodology description about split labels", () => {
    render(<UsageBreakdownsTab data={makeResponse([makeBreakdownRow()])} />);

    expect(
      screen.getByText(
        /Actual API spend and projected subscription-equivalent cost are shown separately/,
      ),
    ).toBeInTheDocument();
  });

  it("sorts by actual_spend by default for user breakdowns", () => {
    render(
      <UsageBreakdownsTab
        data={makeResponse([
          makeBreakdownRow({
            id: "user-low",
            name: "Low spend",
            actual_spend_usd: 5.0,
          }),
          makeBreakdownRow({
            id: "user-high",
            name: "High spend",
            actual_spend_usd: 50.0,
          }),
        ])}
      />,
    );

    // Default sort is desc by actual_spend, so High spend should appear first.
    // Match only the row-name cells (exact "Low spend" / "High spend") so the
    // intro description and column headers that also contain "spend" don't
    // shadow the rows in DOM order.
    const rows = screen.getAllByText(/^(Low|High) spend$/);
    expect(rows[0]).toHaveTextContent("High spend");
  });
});
