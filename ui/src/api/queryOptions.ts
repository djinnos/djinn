import { queryOptions } from "@tanstack/react-query";
import { checkServerHealth, fetchProviderCatalog } from "./server";

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
