import { describe, expect, it } from "vitest";

import type {
  UsageAnalyticsResponse,
  UsageModelEffectiveness,
} from "@/api/analytics";
import { render, screen } from "@/test/test-utils";
import { UsageModelsTab } from "./UsageModelsTab";

function makeModel(
  overrides: Partial<UsageModelEffectiveness> = {},
): UsageModelEffectiveness {
  return {
    model: "openai/gpt-4.1",
    task_count: 10,
    success_rate: 0.9,
    avg_reopens: 0.2,
    cost_per_task: 1.5,
    actual_cost_per_task: 1.0,
    list_price_cost_per_task: 1.5,
    total_cost: 15.0,
    total_tokens: 20000,
    tokens_in: 12000,
    tokens_out: 8000,
    actual_spend_usd: 10.0,
    projected_usd: 5.0,
    list_price_usd: 15.0,
    unpriced_count: 1,
    session_count: 12,
    ...overrides,
  };
}

function makeResponse(
  models: UsageModelEffectiveness[],
): UsageAnalyticsResponse {
  return {
    kpis: [],
    time_series: [],
    breakdowns: {
      by_user: [],
      by_project: [],
      by_proposal: [],
      by_task: [],
    },
    model_effectiveness: models,
    project_model_matrix: [],
    generated_at: "2026-06-25T12:00:00Z",
  };
}

describe("UsageModelsTab — split cost column labels", () => {
  it("renders Actual API spend and Projected subscription-equivalent cost column headers", () => {
    render(<UsageModelsTab data={makeResponse([makeModel()])} />);

    expect(screen.getByText("Actual API spend")).toBeInTheDocument();
    expect(
      screen.getByText("Projected subscription-equivalent cost"),
    ).toBeInTheDocument();
    expect(screen.getByText("Unpriced")).toBeInTheDocument();
  });

  it("renders actual and projected values from split fields", () => {
    render(
      <UsageModelsTab
        data={makeResponse([
          makeModel({
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
      <UsageModelsTab
        data={makeResponse([
          makeModel({
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
      <UsageModelsTab
        data={makeResponse([
          makeModel({
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
    render(<UsageModelsTab data={makeResponse([makeModel()])} />);

    expect(
      screen.getByText(/Actual API spend and projected subscription-equivalent/),
    ).toBeInTheDocument();
  });

  it("renders the combined list-price cost-per-task in the Cost / task column (not em dash)", () => {
    render(
      <UsageModelsTab
        data={makeResponse([
          makeModel({ list_price_cost_per_task: 2.75 }),
        ])}
      />,
    );

    // Regression: the column previously read a never-populated `cost_per_task`
    // field and always showed "—". It must now read list_price_cost_per_task.
    expect(screen.getByText("Cost / task")).toBeInTheDocument();
    expect(screen.getByText("$2.75")).toBeInTheDocument();
  });
});
