import { describe, expect, it } from "vitest";

import type { UsageAnalyticsResponse, UsageProjectModelCell } from "@/api/analytics";
import { render, screen } from "@/test/test-utils";
import {
  buildProjectModelMatrix,
  formatMatrixMetricValue,
  UsageProjectModelMatrixTab,
} from "./UsageProjectModelMatrixTab";

function matrixCell(overrides: Partial<UsageProjectModelCell>): UsageProjectModelCell {
  return {
    project_id: "project-a",
    project_name: "Project Alpha",
    model: "model-a",
    cost_per_task: 1.25,
    success_rate: 0.5,
    avg_reopens: 0.25,
    total_cost: 2.5,
    total_tokens: 1000,
    ...overrides,
  };
}

function analyticsResponse(
  projectModelMatrix: UsageProjectModelCell[],
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
    model_effectiveness: [],
    project_model_matrix: projectModelMatrix,
    generated_at: "2026-01-01T00:00:00Z",
  };
}

describe("UsageProjectModelMatrixTab", () => {
  it("builds project/model axes and leaves missing pairs absent", () => {
    const matrix = buildProjectModelMatrix([
      matrixCell({ project_id: "project-b", project_name: "Project Beta", model: "model-b" }),
      matrixCell({ project_id: "project-a", project_name: "Project Alpha", model: "model-a" }),
    ]);

    expect(matrix.projects.map((project) => project.name)).toEqual([
      "Project Alpha",
      "Project Beta",
    ]);
    expect(matrix.models).toEqual(["model-a", "model-b"]);
    expect(formatMatrixMetricValue(undefined, "cost_per_task")).toBe("—");
  });

  it("renders never-ran cells distinctly from zero-valued and unpriced metrics", () => {
    render(
      <UsageProjectModelMatrixTab
        data={analyticsResponse([
          matrixCell({
            project_id: "project-a",
            project_name: "Project Alpha",
            model: "model-a",
            cost_per_task: null,
            total_cost: null,
            total_tokens: 1234,
          }),
          matrixCell({
            project_id: "project-b",
            project_name: "Project Beta",
            model: "model-b",
            cost_per_task: 0,
            total_cost: 0,
            // Zero *cost* but non-zero tokens: still rendered (only zero-token
            // rows/columns are hidden — see the dedicated test below).
            total_tokens: 500,
          }),
        ])}
      />,
    );

    expect(screen.getByText("Project × Model Matrix")).toBeInTheDocument();
    expect(screen.getByTitle("Project Alpha / model-b: never ran")).toBeInTheDocument();
    expect(screen.getByText("No priced cost/task")).toBeInTheDocument();
    expect(screen.getByText("Tokens 1.2K")).toBeInTheDocument();
    expect(screen.getAllByText("$0.00").length).toBeGreaterThan(0);
  });

  it("hides blank-project rows and zero-token model columns", () => {
    const matrix = buildProjectModelMatrix([
      // Real usage.
      matrixCell({ project_id: "project-a", model: "model-a", total_tokens: 1000 }),
      // Blank project (chat/system) — must not appear as a row.
      matrixCell({ project_id: "", project_name: "", model: "model-a", total_tokens: 50 }),
      // Model that never recorded a token — must not appear as a column.
      matrixCell({ project_id: "project-a", model: "model-zero", total_tokens: 0 }),
    ]);

    expect(matrix.projects.map((p) => p.id)).toEqual(["project-a"]);
    expect(matrix.models).toEqual(["model-a"]);
  });
});
