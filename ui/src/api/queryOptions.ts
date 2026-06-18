import { queryOptions } from "@tanstack/react-query";
import { checkServerHealth, fetchProviderCatalog } from "./server";
import { fetchUsers } from "./users";
import { fetchUserSettings } from "./userSettings";
import {
  fetchUsageAnalytics,
  type UsageAnalyticsFilters,
} from "./analytics";

export const serverHealthQueryOptions = () =>
  queryOptions({
    queryKey: ["server", "health"],
    queryFn: checkServerHealth,
  });

export const providerCatalogQueryOptions = () =>
  queryOptions({
    queryKey: ["providers", "catalog"],
    queryFn: fetchProviderCatalog,
  });

export const usersQueryOptions = () =>
  queryOptions({
    queryKey: ["users", "list"],
    queryFn: fetchUsers,
    // The org roster changes rarely; cache aggressively so the owner
    // filter and creator labels don't refetch on every board mount.
    staleTime: 5 * 60 * 1000,
  });

/**
 * Shared per-user settings query. Used by both the Settings → Models tab and
 * the chat model picker so the user's ordered `models` selection stays in
 * sync across the app (a single cache entry, invalidated on save).
 */
export const USER_SETTINGS_QUERY_KEY = ["user-settings"] as const;

export const userSettingsQueryOptions = () =>
  queryOptions({
    queryKey: USER_SETTINGS_QUERY_KEY,
    queryFn: fetchUserSettings,
    staleTime: 30 * 1000,
  });

/**
 * Admin-only usage analytics query. The query key includes the serialized
 * filter set so React Query deduplicates identical requests regardless of
 * which component mounts the hook.
 */
export const USAGE_ANALYTICS_QUERY_KEY = (filters: UsageAnalyticsFilters) =>
  ["admin", "usage", filters] as const;

export const usageAnalyticsQueryOptions = (filters: UsageAnalyticsFilters) =>
  queryOptions({
    queryKey: USAGE_ANALYTICS_QUERY_KEY(filters),
    queryFn: () => fetchUsageAnalytics(filters),
    staleTime: 60 * 1000,
  });
