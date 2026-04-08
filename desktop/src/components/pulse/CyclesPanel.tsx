import { useQuery } from "@tanstack/react-query";
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { callMcpTool } from "@/api/mcpClient";
import { parseCycles, truncatePathLeft, isPathExcluded } from "./pulseTypes";

interface CyclesPanelProps {
  projectPath: string;
  excludedPaths: string[];
}

function openMember(displayName: string) {
  // TODO: wire to in-app editor open when an editor integration exists.
  console.log("[pulse] open cycle member:", displayName);
}

export function CyclesPanel({ projectPath, excludedPaths }: CyclesPanelProps) {
  const { data, isLoading, error, refetch, isFetching } = useQuery({
    queryKey: ["pulse", "cycles", projectPath],
    queryFn: async () => {
      const raw = await callMcpTool("code_graph", {
        project_path: projectPath,
        operation: "cycles",
        min_size: 2,
      });
      return parseCycles(raw);
    },
    staleTime: 60_000,
  });

  const filtered = (data ?? [])
    .map((g) => ({
      ...g,
      members: g.members.filter(
        (m) => !isPathExcluded(m.display_name || m.key, excludedPaths)
      ),
    }))
    .filter((g) => g.members.length >= 2)
    .sort((a, b) => b.members.length - a.members.length);

  return (
    <Card>
      <CardHeader>
        <CardTitle>Cycles</CardTitle>
        <CardDescription>
          Strongly-connected groups in the dependency graph.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {isLoading ? (
          <div className="space-y-2">
            {Array.from({ length: 3 }).map((_, i) => (
              <Skeleton key={i} className="h-12 w-full" />
            ))}
          </div>
        ) : error ? (
          <div className="flex items-center justify-between gap-3 text-sm">
            <p className="text-muted-foreground">Couldn&apos;t load cycles.</p>
            <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
              Retry
            </Button>
          </div>
        ) : filtered.length === 0 ? (
          <p className="text-sm text-emerald-400/80">No dependency cycles found.</p>
        ) : (
          <div className="space-y-3">
            {filtered.map((group, i) => (
              <div
                key={`${i}-${group.members[0]?.key ?? ""}`}
                className="rounded-lg border border-border/60 bg-muted/20 p-3"
              >
                <div className="mb-2 flex items-center gap-2 text-xs font-medium text-muted-foreground">
                  <span className="rounded-full bg-amber-500/10 px-2 py-0.5 text-amber-400">
                    {group.members.length} members
                  </span>
                </div>
                <ul className="space-y-1">
                  {group.members.map((m) => (
                    <li key={m.key}>
                      <button
                        type="button"
                        onClick={() => openMember(m.display_name || m.key)}
                        className="w-full truncate text-left font-mono text-xs text-foreground/80 hover:text-foreground"
                        title={m.display_name || m.key}
                        dir="rtl"
                      >
                        {truncatePathLeft(m.display_name || m.key)}
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
