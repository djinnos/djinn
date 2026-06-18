import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Analytics01Icon } from "@hugeicons/core-free-icons";
import { usageAnalyticsQueryOptions } from "@/api/queryOptions";
import type { UsageAnalyticsFilters } from "@/api/analytics";
import { InlineError } from "@/components/InlineError";
import { Skeleton } from "@/components/ui/skeleton";

/** Default filters: last 30 days, daily granularity. */
const DEFAULT_FILTERS: UsageAnalyticsFilters = {
  preset: "30d",
  granularity: "day",
};

/**
 * Admin-only usage analytics dashboard. Non-admin access is blocked by the
 * route guard in App.tsx which redirects to /tasks.
 *
 * This is the navigation/routing shell; tab content and filter UI will be
 * added in follow-up tasks.
 */
export function UsageDashboardPage() {
  const { data, isLoading, isError, error, refetch } = useQuery(
    usageAnalyticsQueryOptions(DEFAULT_FILTERS),
  );

  return (
    <div className="flex h-full flex-col overflow-hidden p-6">
      <div className="mb-5 flex items-center gap-3">
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

      <div className="min-h-0 flex-1 overflow-y-auto pb-6">
        {isLoading ? (
          <div className="space-y-3">
            <Skeleton className="h-10 w-full rounded-lg" />
            <div className="grid grid-cols-4 gap-3">
              {Array.from({ length: 4 }).map((_, i) => (
                <Skeleton key={i} className="h-24 rounded-lg" />
              ))}
            </div>
            <Skeleton className="h-64 w-full rounded-lg" />
          </div>
        ) : isError ? (
          <InlineError
            message={
              error instanceof Error
                ? error.message
                : "Failed to load usage analytics"
            }
            onRetry={() => void refetch()}
          />
        ) : (
          <div className="space-y-6">
            {/* Placeholder KPI row — will be filled in follow-up tasks */}
            {data?.kpis && data.kpis.length > 0 ? (
              <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
                {data.kpis.map((kpi) => (
                  <div
                    key={kpi.label}
                    className="rounded-lg border border-border bg-card p-4"
                  >
                    <p className="text-xs text-muted-foreground">{kpi.label}</p>
                    <p className="mt-1 text-lg font-semibold text-foreground">
                      {kpi.formatted}
                    </p>
                    {kpi.delta_pct !== null && (
                      <p
                        className={`mt-0.5 text-xs ${
                          kpi.delta_pct >= 0
                            ? "text-emerald-500"
                            : "text-red-400"
                        }`}
                      >
                        {kpi.delta_pct >= 0 ? "+" : ""}
                        {kpi.delta_pct.toFixed(1)}%
                      </p>
                    )}
                  </div>
                ))}
              </div>
            ) : (
              <div className="rounded-lg border border-dashed border-border bg-card/50 px-4 py-10 text-center text-sm text-muted-foreground">
                Analytics data will appear here once the backend is ready.
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
