import { useQuery } from "@tanstack/react-query";
import { callMcpTool } from "@/api/mcpClient";

/**
 * True when an architect-role session is currently dispatched or running
 * for the given project. Polls session_active every 10s when the tab is
 * visible. Returns false on error or when no project slug is available.
 */
export function useArchitectActive(projectSlug: string | null | undefined): boolean {
  const { data } = useQuery({
    queryKey: ["pulse", "architect-active", projectSlug],
    queryFn: async () => {
      const result = await callMcpTool("session_active", { project: projectSlug! });
      const sessions = result.sessions ?? [];
      return sessions.some((s) => {
        const role = (s.agent_type ?? "").toLowerCase();
        const status = (s.status ?? "").toLowerCase();
        const live = status === "dispatched" || status === "running" || status === "active" || status === "started";
        return role === "architect" && live;
      });
    },
    enabled: !!projectSlug,
    staleTime: 10_000,
    refetchInterval: 10_000,
    refetchOnWindowFocus: true,
  });

  return data ?? false;
}
