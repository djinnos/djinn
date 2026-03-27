import { QueryClient, type InvalidateOptions, type InvalidateQueryFilters, type QueryKey } from "@tanstack/react-query";

const SSE_QUERY_DEBOUNCE_MS = 150;

type DebouncedInvalidateOptions = {
  debounceMs?: number;
};

type PendingInvalidation = {
  filters: InvalidateQueryFilters;
  options?: InvalidateOptions;
  timer: ReturnType<typeof setTimeout>;
};

export const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 30_000,
      gcTime: 5 * 60_000,
      retry: 1,
      refetchOnWindowFocus: false,
      refetchOnReconnect: true,
    },
  },
});

const pendingInvalidations = new Map<string, PendingInvalidation>();

function serializeQueryKey(queryKey: QueryKey): string {
  return JSON.stringify(queryKey);
}

function runInvalidation(filters: InvalidateQueryFilters, options?: InvalidateOptions): void {
  void queryClient.invalidateQueries(filters, options);
}

export function debounceInvalidateQueries(
  filters: InvalidateQueryFilters,
  options?: InvalidateOptions,
  debounceOptions: DebouncedInvalidateOptions = {},
): void {
  const queryKey = filters.queryKey;
  if (!queryKey) {
    runInvalidation(filters, options);
    return;
  }

  const debounceMs = debounceOptions.debounceMs ?? SSE_QUERY_DEBOUNCE_MS;
  const pendingKey = serializeQueryKey(queryKey);
  const existingPending = pendingInvalidations.get(pendingKey);
  if (existingPending) {
    clearTimeout(existingPending.timer);
  }

  const timer = setTimeout(() => {
    const pending = pendingInvalidations.get(pendingKey);
    pendingInvalidations.delete(pendingKey);
    runInvalidation(pending?.filters ?? filters, pending?.options ?? options);
  }, debounceMs);

  pendingInvalidations.set(pendingKey, { filters, options, timer });
}

export function flushDebouncedInvalidations(): void {
  const pendingEntries = Array.from(pendingInvalidations.values());
  pendingInvalidations.clear();

  for (const pending of pendingEntries) {
    clearTimeout(pending.timer);
    runInvalidation(pending.filters, pending.options);
  }
}

export { SSE_QUERY_DEBOUNCE_MS };
