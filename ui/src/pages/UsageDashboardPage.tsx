import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Analytics01Icon, Refresh01Icon } from "@hugeicons/core-free-icons";
import { usageAnalyticsQueryOptions } from "@/api/queryOptions";
import type { UsageAnalyticsFilters } from "@/api/analytics";
import { InlineError } from "@/components/InlineError";
import { Skeleton } from "@/components/ui/skeleton";
import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { UsageDashboardFiltersBar } from "@/components/usage/UsageDashboardFilters";
import { UsageBreakdownsTab } from "@/components/usage/UsageBreakdownsTab";
import { UsageModelsTab } from "@/components/usage/UsageModelsTab";
import { UsageOverviewTab } from "@/components/usage/UsageOverviewTab";
import { UsageProjectModelMatrixTab } from "@/components/usage/UsageProjectModelMatrixTab";
import { cn } from "@/lib/utils";

/** Default filters: last 30 days, daily granularity. */
const DEFAULT_FILTERS: UsageAnalyticsFilters = {
  preset: "30d",
  granularity: "day",
};

/** Tab values for the dashboard. */
const TAB_OVERVIEW = "overview";
const TAB_MODELS = "models";
const TAB_BREAKDOWNS = "breakdowns";
const TAB_MATRIX = "matrix";

/**
 * Admin-only usage analytics dashboard. Non-admin access is blocked by the
 * route guard in App.tsx which redirects to /tasks.
 *
 * The page owns a single canonical `UsageAnalyticsFilters` object that is
 * passed to the React Query option and consumed by every tab, so all tabs
 * share the same fetched data and there is no per-tab duplicated filter
 * logic or divergent request.
 */
export function UsageDashboardPage() {
  const [filters, setFilters] =
    useState<UsageAnalyticsFilters>(DEFAULT_FILTERS);

  const { data, isLoading, isError, error, refetch, isFetching } = useQuery({
    ...usageAnalyticsQueryOptions(filters),
    // Keep the previous result visible while refetching on a filter change so
    // the dashboard never flashes to a loading skeleton unnecessarily and the
    // active tab is not lost.
    placeholderData: (prev) => prev,
  });

  // ── Empty-state detection ─────────────────────────────────────────────────
  const isEmpty = useMemo(() => {
    if (!data) return false;
    return (
      data.kpis.length === 0 &&
      data.time_series.length === 0 &&
      data.model_effectiveness.length === 0 &&
      data.project_model_matrix.length === 0 &&
      data.breakdowns.by_user.length === 0 &&
      data.breakdowns.by_project.length === 0 &&
      data.breakdowns.by_proposal.length === 0 &&
      data.breakdowns.by_task.length === 0
    );
  }, [data]);

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      {/* ── Header ─────────────────────────────────────────────────────────── */}
      <div className="mb-4 flex items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <span className="flex h-9 w-9 items-center justify-center rounded-lg bg-muted text-muted-foreground">
            <HugeiconsIcon icon={Analytics01Icon} size={18} />
          </span>
          <div>
            <h1 className="text-xl font-bold text-foreground">
              Usage &amp; Analytics
            </h1>
            <p className="text-sm text-muted-foreground">
              AI usage, cost, and effectiveness across this deployment.
            </p>
          </div>
        </div>
        <Button
          variant="outline"
          size="sm"
          className="shrink-0"
          onClick={() => void refetch()}
          disabled={isFetching}
        >
          <HugeiconsIcon
            icon={Refresh01Icon}
            size={14}
            className={cn("mr-1.5", isFetching && "animate-spin")}
          />
          Refresh
        </Button>
      </div>

      {/* ── Global filter bar ──────────────────────────────────────────────── */}
      <UsageDashboardFiltersBar
        filters={filters}
        onChange={setFilters}
        data={data}
      />

      {/* ── Content ────────────────────────────────────────────────────────── */}
      <div className="min-h-0 flex-1 overflow-y-auto pt-5 pb-6">
        {isLoading ? (
          <DashboardSkeleton />
        ) : isError ? (
          <InlineError
            message={
              error instanceof Error
                ? error.message
                : "Failed to load usage analytics"
            }
            onRetry={() => void refetch()}
            retrying={isFetching}
          />
        ) : isEmpty ? (
          <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-12 text-center">
            <p className="text-sm font-medium text-foreground">No usage data</p>
            <p className="mt-1 text-sm text-muted-foreground">
              Analytics will appear here once the deployment has completed task
              activity within the selected range.
            </p>
          </div>
        ) : (
          <Tabs defaultValue={TAB_OVERVIEW} className="flex flex-col gap-4">
            <TabsList className="w-fit">
              <TabsTrigger value={TAB_OVERVIEW}>Overview</TabsTrigger>
              <TabsTrigger value={TAB_MODELS}>Models</TabsTrigger>
              <TabsTrigger value={TAB_BREAKDOWNS}>Breakdowns</TabsTrigger>
              <TabsTrigger value={TAB_MATRIX}>
                Project × Model Matrix
              </TabsTrigger>
            </TabsList>

            <TabsContent value={TAB_OVERVIEW}>
              {data && <UsageOverviewTab data={data} />}
            </TabsContent>

            <TabsContent value={TAB_MODELS}>
              {data && <UsageModelsTab data={data} />}
            </TabsContent>

            <TabsContent value={TAB_BREAKDOWNS}>
              {data && <UsageBreakdownsTab data={data} />}
            </TabsContent>

            <TabsContent value={TAB_MATRIX}>
              {data && <UsageProjectModelMatrixTab data={data} />}
            </TabsContent>
          </Tabs>
        )}
      </div>
    </div>
  );
}

// ── Loading skeleton ────────────────────────────────────────────────────────

function DashboardSkeleton() {
  return (
    <div className="space-y-4">
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        {Array.from({ length: 4 }).map((_, i) => (
          <Skeleton key={i} className="h-24 rounded-lg" />
        ))}
      </div>
      <Skeleton className="h-64 w-full rounded-lg" />
      <Skeleton className="h-48 w-full rounded-lg" />
    </div>
  );
}
