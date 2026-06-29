import { describe, expect, it, vi } from "vitest";

import type {
  UsageAnalyticsResponse,
  UsageKpi,
  UsageTimeSeriesPoint,
} from "@/api/analytics";
import { render, screen } from "@/test/test-utils";
import { UsageOverviewTab } from "./UsageOverviewTab";

// Mock @nivo/bar — the charting library renders SVGs that don't fully work in
// jsdom. We only care about the surrounding labels and KPI cards, not the bars.
vi.mock("@nivo/bar", () => ({
  ResponsiveBar: () => <div data-testid="mock-bar-chart" />,
}));

function makeResponse(overrides: Partial<UsageAnalyticsResponse> = {}): UsageAnalyticsResponse {
  return {
    kpis: [],
    time_series: [],
    breakdowns: {
      by_user: [],
      by_project: [],
      by_proposal: [],
      by_task: [],
    },
    model_effectiveness: [],
    project_model_matrix: [],
    generated_at: "2026-06-25T12:00:00Z",
    ...overrides,
  };
}

function makeKpi(overrides: Partial<UsageKpi> = {}): UsageKpi {
  return {
    label: "Spend",
    value: 42.5,
    delta_pct: 0.12,
    formatted: "$42.50",
    actual_spend_usd: 30.0,
    projected_usd: 12.5,
    unpriced_count: 3,
    ...overrides,
  };
}

describe("UsageOverviewTab — split cost labels", () => {
  it("renders KPI cards with split labels: Actual API spend, Projected subscription-equivalent cost, and Unpriced sessions", () => {
    render(
      <UsageOverviewTab
        data={makeResponse({ kpis: [makeKpi()] })}
      />,
    );

    // Use stable data-testid selectors to avoid ambiguity with labels that
    // appear in multiple DOM locations (KPI cards, chart section, disclosure).
    expect(screen.getByTestId("usage-split-kpi-actual-spend")).toHaveTextContent("Actual API spend");
    expect(screen.getByTestId("usage-split-kpi-projected-cost")).toHaveTextContent("Projected subscription-equivalent cost");
    expect(screen.getByTestId("usage-split-kpi-unpriced-sessions")).toHaveTextContent("Unpriced sessions");
  });

  it("exposes stable data-testid selectors on the three split KPI cards", () => {
    render(
      <UsageOverviewTab
        data={makeResponse({ kpis: [makeKpi()] })}
      />,
    );

    expect(
      screen.getByTestId("usage-split-kpi-actual-spend"),
    ).toBeInTheDocument();
    expect(
      screen.getByTestId("usage-split-kpi-projected-cost"),
    ).toBeInTheDocument();
    expect(
      screen.getByTestId("usage-split-kpi-unpriced-sessions"),
    ).toBeInTheDocument();
  });

  it("renders Actual API spend KPI value from split field", () => {
    render(
      <UsageOverviewTab
        data={makeResponse({ kpis: [makeKpi({ actual_spend_usd: 123.45 })] })}
      />,
    );

    expect(screen.getByText("$123")).toBeInTheDocument();
  });

  it("renders Projected subscription-equivalent cost KPI value from split field", () => {
    render(
      <UsageOverviewTab
        data={makeResponse({ kpis: [makeKpi({ projected_usd: 67.89 })] })}
      />,
    );

    expect(screen.getByTestId("usage-split-kpi-projected-cost")).toHaveTextContent("$67.89");
  });

  it("shows em dash for both cost KPIs when split fields are absent (pre-split response)", () => {
    const kpi: UsageKpi = {
      label: "Spend",
      value: 100,
      delta_pct: null,
      formatted: "$100",
      // no split fields
    };

    render(<UsageOverviewTab data={makeResponse({ kpis: [kpi] })} />);

    // Both cost KPI cards should show em dash
    const dashes = screen.getAllByText("—");
    expect(dashes.length).toBeGreaterThanOrEqual(2);
  });

  it("renders unpriced count as integer, never as $0", () => {
    render(
      <UsageOverviewTab
        data={makeResponse({
          kpis: [makeKpi({ unpriced_count: 7 })],
          unpriced_session_count: 7,
        })}
      />,
    );

    // Use stable testId selector to scope to the unpriced card
    const unpricedCard = screen.getByTestId("usage-split-kpi-unpriced-sessions");
    expect(unpricedCard).toHaveTextContent("7");
    // The label should say "Unpriced sessions" not any dollar value
    expect(unpricedCard).toHaveTextContent("Unpriced sessions");
    expect(unpricedCard).toHaveTextContent("Excluded from both cost figures");
  });

  it("renders the chart section with separate actual and projected labels", () => {
    render(<UsageOverviewTab data={makeResponse({ kpis: [makeKpi()] })} />);

    expect(screen.getByText("Cost over time")).toBeInTheDocument();
    // "Actual API spend" and "Projected subscription-equivalent cost" appear in
    // multiple DOM locations (KPI cards, chart section, disclosure), so assert
    // at least one match exists via getAllByText.
    expect(screen.getAllByText("Actual API spend").length).toBeGreaterThanOrEqual(1);
    expect(
      screen.getAllByText("Projected subscription-equivalent cost").length,
    ).toBeGreaterThanOrEqual(1);
    expect(
      screen.getByText(
        "Actual API spend and projected subscription-equivalent cost shown separately. Unpriced sessions are excluded from both figures.",
      ),
    ).toBeInTheDocument();
  });

  it("renders split bar charts for Cost by model and Cost by agent role", () => {
    render(<UsageOverviewTab data={makeResponse({ kpis: [makeKpi()] })} />);

    expect(screen.getByText("Cost by model")).toBeInTheDocument();
    expect(screen.getByText("Cost by agent role")).toBeInTheDocument();
  });

  it("renders the methodology disclosure explaining actual, projected, and unpriced semantics", () => {
    render(<UsageOverviewTab data={makeResponse({ kpis: [makeKpi()] })} />);

    expect(screen.getByText("How costs are calculated")).toBeInTheDocument();
    // Labels appear in multiple DOM locations; assert they exist via getAllByText
    expect(screen.getAllByText("Actual API spend").length).toBeGreaterThanOrEqual(1);
    expect(
      screen.getAllByText("Projected subscription-equivalent cost").length,
    ).toBeGreaterThanOrEqual(1);
    expect(screen.getAllByText("Unpriced sessions").length).toBeGreaterThanOrEqual(1);
    // Verify the methodology text explains list-price projection
    expect(
      screen.getByText(/is a list-price equivalent estimate/i),
    ).toBeInTheDocument();
    // Verify the disclosure explicitly says projected is not a spend figure
    expect(
      screen.getByText(/is not a spend figure/i),
    ).toBeInTheDocument();
    // Verify unpriced explanation
    expect(
      screen.getByText(
        /are excluded from both figures because they had no matching catalog pricing/i,
      ),
    ).toBeInTheDocument();
  });
});

describe("UsageOverviewTab — split cost time-series", () => {
  it("renders time-series chart labels correctly", () => {
    const points: UsageTimeSeriesPoint[] = [
      {
        date: "2026-06-01",
        cost: 5.0,
        tokens_in: 100,
        tokens_out: 50,
        task_count: 1,
        actual_spend_usd: 3.0,
        projected_usd: 2.0,
        unpriced_count: 0,
      },
    ];

    render(
      <UsageOverviewTab
        data={makeResponse({
          kpis: [makeKpi()],
          time_series: points,
        })}
      />,
    );

    expect(screen.getByText("Cost over time")).toBeInTheDocument();
    // Labels appear in multiple DOM locations; assert they exist via getAllByText
    expect(screen.getAllByText("Actual API spend").length).toBeGreaterThanOrEqual(1);
    expect(
      screen.getAllByText("Projected subscription-equivalent cost").length,
    ).toBeGreaterThanOrEqual(1);
  });
});

describe("UsageOverviewTab — split KPI render smoke", () => {
  /**
   * Render smoke for proposal zhwn: a priced/mixed usage window must render
   * Actual API spend, Projected subscription-equivalent cost, and Unpriced
   * sessions as user-visible values rather than fallback placeholders.
   *
   * Fixture shape matches the repaired/generated `/api/admin/usage` contract
   * from tp93 with separate KPI contributors carrying split fields.
   */
  it("renders all three split KPI cards with currency/count values from the repaired contract shape, not fallback placeholders", () => {
    // Build a UsageAnalyticsResponse fixture matching the repaired contract
    // shape with separate KPI contributors.
    const actualKpi: UsageKpi = {
      label: "Total Spend",
      value: 69.12,
      delta_pct: null,
      formatted: "$69.12",
      actual_spend_usd: 12.34,
      projected_usd: 0,
      unpriced_count: 0,
    };

    const projectedKpi: UsageKpi = {
      label: "Projected Cost",
      value: 56.78,
      delta_pct: null,
      formatted: "$56.78",
      actual_spend_usd: 0,
      projected_usd: 56.78,
      unpriced_count: 0,
    };

    const unpricedKpi: UsageKpi = {
      label: "Sessions",
      value: 3,
      delta_pct: null,
      formatted: "3",
      actual_spend_usd: 0,
      projected_usd: 0,
      unpriced_count: 3,
    };

    render(
      <UsageOverviewTab
        data={makeResponse({
          kpis: [actualKpi, projectedKpi, unpricedKpi],
          unpriced_session_count: 3,
        })}
      />,
    );

    // Use stable data-testid selectors scoped to each KPI card.
    const actualCard = screen.getByTestId("usage-split-kpi-actual-spend");
    const projectedCard = screen.getByTestId("usage-split-kpi-projected-cost");
    const unpricedCard = screen.getByTestId("usage-split-kpi-unpriced-sessions");

    // Actual API spend card renders currency from actual_spend_usd (not —)
    expect(actualCard).toHaveTextContent("$12.34");
    expect(actualCard).not.toHaveTextContent("—");

    // Projected subscription-equivalent cost card renders currency from projected_usd (not —)
    expect(projectedCard).toHaveTextContent("$56.78");
    expect(projectedCard).not.toHaveTextContent("—");

    // Unpriced sessions card renders the seeded backend count (not fallback 0)
    expect(unpricedCard).toHaveTextContent("3");
    expect(unpricedCard).not.toHaveTextContent("0");
  });
});
