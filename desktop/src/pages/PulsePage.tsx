import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Pulse01Icon } from "@hugeicons/core-free-icons";
import { useSelectedProject } from "@/stores/useProjectStore";
import { callMcpTool } from "@/api/mcpClient";
import { FreshnessStrip } from "@/components/pulse/FreshnessStrip";
import { useArchitectActive } from "@/hooks/useArchitectActive";
import { cn } from "@/lib/utils";

interface PulseStatus {
  project_id: string;
  warmed: boolean;
  last_warm_at: string | null;
  pinned_commit: string | null;
  commits_since_pin: number | null;
}

function isPulseStatus(value: unknown): value is PulseStatus {
  if (!value || typeof value !== "object") return false;
  const v = value as Record<string, unknown>;
  return typeof v.warmed === "boolean";
}

function ProjectEmptyState() {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="max-w-sm text-center">
        <HugeiconsIcon
          icon={Pulse01Icon}
          className="mx-auto h-10 w-10 text-muted-foreground/40"
        />
        <p className="mt-4 text-sm text-muted-foreground">
          Select a project to view its pulse.
        </p>
      </div>
    </div>
  );
}

function NotWarmedState() {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="max-w-md text-center">
        <span
          className={cn(
            "mx-auto flex h-14 w-14 items-center justify-center rounded-full",
            "bg-muted/30 text-muted-foreground/60 animate-pulse [animation-duration:4s]"
          )}
        >
          <HugeiconsIcon icon={Pulse01Icon} className="h-6 w-6" />
        </span>
        <h2 className="mt-5 text-base font-medium text-foreground">Pulse not ready</h2>
        <p className="mt-2 text-sm text-muted-foreground">
          The architect hasn&apos;t patrolled this codebase yet. Pulse will populate
          after the next architect dispatch.
        </p>
      </div>
    </div>
  );
}

function WarmingState() {
  return (
    <div className="flex h-full items-center justify-center">
      <div className="max-w-md text-center">
        <span
          className={cn(
            "mx-auto flex h-14 w-14 items-center justify-center rounded-full",
            "bg-emerald-500/10 text-emerald-400 animate-pulse [animation-duration:1.1s]"
          )}
        >
          <HugeiconsIcon icon={Pulse01Icon} className="h-6 w-6" />
        </span>
        <h2 className="mt-5 text-base font-medium text-foreground">
          Architect is patrolling your codebase…
        </h2>
        <p className="mt-2 text-sm text-muted-foreground">
          Reading symbols, computing centrality, mapping dependencies. This usually
          takes ~30 seconds.
        </p>
      </div>
    </div>
  );
}

export function PulsePage() {
  const project = useSelectedProject();
  const projectPath = project?.path ?? null;
  const architectActive = useArchitectActive(projectPath);

  const { data, isLoading } = useQuery({
    queryKey: ["pulse", "status", projectPath],
    queryFn: async () => {
      const result = await callMcpTool("code_graph", {
        project_path: projectPath!,
        operation: "status",
      });
      return isPulseStatus(result) ? result : null;
    },
    enabled: !!projectPath,
    staleTime: 30_000,
    refetchInterval: 30_000,
    refetchOnWindowFocus: true,
  });

  if (!project) {
    return <ProjectEmptyState />;
  }

  if (isLoading && !data) {
    return (
      <div className="flex h-full items-center justify-center">
        <p className="text-sm text-muted-foreground">Loading pulse…</p>
      </div>
    );
  }

  const warmed = data?.warmed ?? false;

  if (!warmed && architectActive) {
    return <WarmingState />;
  }

  if (!warmed) {
    return <NotWarmedState />;
  }

  return (
    <div className="flex h-full min-h-0 flex-col gap-4 overflow-y-auto p-4">
      <FreshnessStrip
        lastWarmAt={data?.last_warm_at ?? null}
        pinnedCommit={data?.pinned_commit ?? null}
        commitsSincePin={data?.commits_since_pin ?? null}
        architectActive={architectActive}
      />
      {/* PULSE_PANELS_SLOT — Phase 3 fills this with Hotspots, Dead code, Cycles, Blast radius */}
      <div className="flex-1" />
    </div>
  );
}
