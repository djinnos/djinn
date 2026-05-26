import { queryOptions } from "@tanstack/react-query";
import { checkServerHealth, fetchProviderCatalog } from "./server";
import { fetchUsers } from "./users";

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
