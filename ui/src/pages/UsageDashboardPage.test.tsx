import { describe, it, expect, vi, beforeEach } from "vitest";
import { act, waitFor } from "@testing-library/react";
import { render } from "@/test/test-utils";
import { UsageDashboardPage } from "./UsageDashboardPage";
import type { UsageAnalyticsFilters } from "@/api/analytics";

// ── Mocks ─────────────────────────────────────────────────────────────────────

// Track the props passed to UsageDashboardFiltersBar so we can assert the
// page-level URL-state contract without rendering the real filter controls.
let lastFiltersBarProps: {
  filters: UsageAnalyticsFilters;
  onChange: (next: UsageAnalyticsFilters) => void;
} | null = null;

vi.mock("@/components/usage/UsageDashboardFilters", () => ({
  UsageDashboardFiltersBar: ({
    filters,
    onChange,
  }: {
    filters: UsageAnalyticsFilters;
    onChange: (next: UsageAnalyticsFilters) => void;
  }) => {
    lastFiltersBarProps = { filters, onChange };
    return <div data-testid="filters-bar" />;
  },
}));

// Mock the heavy tab components — we only care about URL ↔ filter flow.
vi.mock("@/components/usage/UsageOverviewTab", () => ({
  UsageOverviewTab: () => <div data-testid="overview-tab" />,
}));
vi.mock("@/components/usage/UsageModelsTab", () => ({
  UsageModelsTab: () => <div data-testid="models-tab" />,
}));
vi.mock("@/components/usage/UsageBreakdownsTab", () => ({
  UsageBreakdownsTab: () => <div data-testid="breakdowns-tab" />,
}));
vi.mock("@/components/usage/UsageProjectModelMatrixTab", () => ({
  UsageProjectModelMatrixTab: () => <div data-testid="matrix-tab" />,
}));

// Control variable: set to true before rendering to get an empty-data response.
let mockReturnEmptyData = false;

// Provide a minimal stub for the analytics query so the page never actually
// fetches. The stub returns data that satisfies the non-empty check by default,
// or empty arrays when mockReturnEmptyData is true.
vi.mock("@/api/queryOptions", () => ({
  usageAnalyticsQueryOptions: (filters: UsageAnalyticsFilters) => ({
    queryKey: ["admin", "usage", filters],
    queryFn: () =>
      Promise.resolve(
        mockReturnEmptyData
          ? {
              kpis: [],
              time_series: [],
              breakdowns: { by_user: [], by_project: [], by_proposal: [], by_task: [] },
              model_effectiveness: [],
              project_model_matrix: [],
              generated_at: "2026-06-25T00:00:00Z",
            }
          : {
              kpis: [{ label: "Tasks", value: 42, delta_pct: null, formatted: "42" }],
              time_series: [],
              breakdowns: { by_user: [], by_project: [], by_proposal: [], by_task: [] },
              model_effectiveness: [],
              project_model_matrix: [],
              generated_at: "2026-06-25T00:00:00Z",
            },
      ),
  }),
}));

beforeEach(() => {
  lastFiltersBarProps = null;
  mockReturnEmptyData = false;
});

// ── URL-state restore ─────────────────────────────────────────────────────────

describe("UsageDashboardPage — URL-state restore", () => {
  it("restores filter state from URL search params and passes into FiltersBar", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/admin/usage?preset=7d&granularity=week&model=openai%2Fgpt-4.1"],
        },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    expect(lastFiltersBarProps!.filters).toEqual(
      expect.objectContaining({
        preset: "7d",
        granularity: "week",
        model: "openai/gpt-4.1",
      }),
    );
  });

  it("defaults to 30d/daily when no URL params are present", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: { initialEntries: ["/admin/usage"] },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    expect(lastFiltersBarProps!.filters.preset).toBe("30d");
    expect(lastFiltersBarProps!.filters.granularity).toBe("day");
  });

  it("restores a project_id filter as a reporting filter (not global context)", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/admin/usage?preset=30d&project_id=proj-123"],
        },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    expect(lastFiltersBarProps!.filters.project_id).toBe("proj-123");
  });

  it("restores all supported URL keys together", async () => {
    const qs = [
      "preset=7d",
      "granularity=week",
      "project_id=proj-abc",
      "model=openai%2Fgpt-4.1",
      "agent_type=worker",
      "user_id=user-99",
    ].join("&");

    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: { initialEntries: [`/admin/usage?${qs}`] },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    expect(lastFiltersBarProps!.filters).toEqual(
      expect.objectContaining({
        preset: "7d",
        granularity: "week",
        project_id: "proj-abc",
        model: "openai/gpt-4.1",
        agent_type: "worker",
        user_id: "user-99",
      }),
    );
  });
});

// ── Filter change → URL update ────────────────────────────────────────────────

describe("UsageDashboardPage — filter changes update URL", () => {
  it("updates URL search params for a non-date filter change (model)", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/admin/usage?preset=30d"],
        },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    // Simulate the filter bar calling onChange with a new model filter.
    // This exercises the handleFiltersChange callback → setSearchParams path.
    act(() => {
      lastFiltersBarProps!.onChange({
        preset: "30d",
        granularity: "day",
        model: "anthropic/claude-sonnet-4-20250514",
      });
    });

    await waitFor(() => {
      // After onChange the page should have updated its internal state; the
      // next render passes the new filters into the filter bar.
      expect(lastFiltersBarProps!.filters.model).toBe(
        "anthropic/claude-sonnet-4-20250514",
      );
    });
  });

  it("updates URL search params for a date/granularity path change", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/admin/usage?preset=30d&granularity=day"],
        },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    // Switch to weekly granularity and 7d preset — exercises the date path.
    act(() => {
      lastFiltersBarProps!.onChange({
        preset: "7d",
        granularity: "week",
      });
    });

    await waitFor(() => {
      expect(lastFiltersBarProps!.filters.preset).toBe("7d");
      expect(lastFiltersBarProps!.filters.granularity).toBe("week");
    });
  });

  it("updates URL when switching from preset to custom date range", async () => {
    render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: {
          initialEntries: ["/admin/usage?preset=30d"],
        },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    // Switch to custom date range — exercises the preset→custom date path.
    act(() => {
      lastFiltersBarProps!.onChange({
        start: "2026-01-01",
        end: "2026-03-31",
        granularity: "month",
      });
    });

    await waitFor(() => {
      expect(lastFiltersBarProps!.filters.preset).toBeUndefined();
      expect(lastFiltersBarProps!.filters.start).toBe("2026-01-01");
      expect(lastFiltersBarProps!.filters.end).toBe("2026-03-31");
    });
  });
});

// ── Empty state renders shared EmptyState ─────────────────────────────────────

describe("UsageDashboardPage — empty state", () => {
  it("renders shared EmptyState with title and Refresh action when query returns empty data", async () => {
    mockReturnEmptyData = true;

    const { getByText, getAllByRole } = render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: { initialEntries: ["/admin/usage?preset=30d"] },
      },
    });

    // The shared EmptyState renders the title as an <h3>.
    await waitFor(() => {
      expect(getByText("No usage data")).toBeTruthy();
    });

    // The descriptive message is present.
    expect(
      getByText(/Analytics will appear here once the deployment/),
    ).toBeTruthy();

    // The Refresh action button is rendered by the shared EmptyState (the
    // header also has a Refresh button, so there should be at least two).
    const refreshButtons = getAllByRole("button", { name: "Refresh" });
    expect(refreshButtons.length).toBeGreaterThanOrEqual(2);
  });

  it("keeps the filter bar visible when the empty state is shown", async () => {
    mockReturnEmptyData = true;

    const { getByTestId } = render(<UsageDashboardPage />, {
      wrapperOptions: {
        routerProps: { initialEntries: ["/admin/usage?preset=30d"] },
      },
    });

    await waitFor(() => {
      expect(lastFiltersBarProps).not.toBeNull();
    });

    // The mocked filter bar placeholder is still in the DOM.
    expect(getByTestId("filters-bar")).toBeTruthy();
  });
});

// ── Source-level guard: no global project store import ────────────────────────

describe("UsageDashboardPage — source-level guardrails", () => {
  it("does not import useProjectStore (static analysis)", async () => {
    // Read the raw source file and assert it does not reference the global
    // project store.  This is a compile-time guard enforced at test-time.
    const source = await import("./UsageDashboardPage?raw");
    const text: string = source.default ?? "";

    expect(text).not.toMatch(/useProjectStore/);
    expect(text).not.toMatch(/useSelectedProject/);
    expect(text).not.toMatch(/useSelectedProjectId/);
    expect(text).not.toMatch(/projectStore/);
  });
});
