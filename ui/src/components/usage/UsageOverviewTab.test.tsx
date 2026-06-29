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

    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
    expect(screen.getByText("Unpriced sessions")).toBeInTheDocument();
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

    expect(screen.getByText("$68")).toBeInTheDocument();
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

    expect(screen.getByText("7")).toBeInTheDocument();
    // The label should say "Unpriced sessions" not any dollar value
    expect(screen.getByText("Unpriced sessions")).toBeInTheDocument();
    expect(
      screen.getByText("Excluded from both cost figures"),
    ).toBeInTheDocument();
  });

  it("renders the chart section with separate actual and projected labels", () => {
    render(<UsageOverviewTab data={makeResponse({ kpis: [makeKpi()] })} />);

    expect(screen.getByText("Cost over time")).toBeInTheDocument();
    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
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
    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
    expect(screen.getByText("Unpriced sessions")).toBeInTheDocument();
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
    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
  });
});
