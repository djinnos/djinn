import { useQueries } from "@tanstack/react-query";
import { useProjectStore, useProjects, useSelectedProjectId } from "@/stores/useProjectStore";
import { fetchDevcontainerStatus, type DevcontainerStatus } from "@/api/devcontainer";
import { cn } from "@/lib/utils";

type WarmTone = "ready" | "running" | "pending" | "failed" | "unknown";

interface WarmDescriptor {
  tone: WarmTone;
  label: string;
}

function describeWarmState(status: DevcontainerStatus | undefined): WarmDescriptor {
  if (!status) return { tone: "unknown", label: "…" };
  if (status.image_status === "failed") return { tone: "failed", label: "Image failed" };
  if (status.image_status === "building") return { tone: "running", label: "Building image" };

  switch (status.graph_warm_status) {
    case "ready":
      return { tone: "ready", label: "Warmed" };
    case "running":
      return { tone: "running", label: "Warming" };
    case "failed":
      return { tone: "failed", label: "Warm failed" };
    case "pending":
    default:
      return { tone: "pending", label: "Cold" };
  }
}

const toneDot: Record<WarmTone, string> = {
  ready: "bg-emerald-400/90",
  running: "bg-amber-400/90 animate-pulse [animation-duration:2s]",
  pending: "bg-muted-foreground/50",
  failed: "bg-red-400/90",
  unknown: "bg-muted-foreground/30",
};

export function ProjectPulsePicker() {
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const setSelectedProjectId = useProjectStore((state) => state.setSelectedProjectId);

  const statusResults = useQueries({
    queries: projects.map((project) => ({
      queryKey: ["devcontainer", "status", project.id] as const,
      queryFn: () => fetchDevcontainerStatus(project.id),
      staleTime: 15_000,
      refetchInterval: 15_000,
    })),
  });

  const statusById = new Map<string, DevcontainerStatus | undefined>();
  projects.forEach((project, i) => {
    statusById.set(project.id, statusResults[i]?.data as DevcontainerStatus | undefined);
  });

  if (projects.length === 0) {
    return (
      <div className="border-b border-border/60 bg-background/40 px-4 py-2.5 text-sm text-muted-foreground">
        No projects yet. Add one from the Repositories page.
      </div>
    );
  }

  return (
    <div className="flex shrink-0 items-center gap-2 overflow-x-auto border-b border-border/60 bg-background/40 px-4 py-2.5">
      <span className="shrink-0 text-xs uppercase tracking-wide text-muted-foreground/70">Project</span>
      <div className="flex items-center gap-1.5">
        {projects.map((project) => {
          const isSelected = project.id === selectedProjectId;
          const descriptor = describeWarmState(statusById.get(project.id));
          return (
            <button
              key={project.id}
              type="button"
              onClick={() => setSelectedProjectId(project.id)}
              className={cn(
                "group flex shrink-0 items-center gap-2 rounded-full border px-3 py-1 text-sm transition-colors",
                isSelected
                  ? "border-border bg-white/[0.06] text-foreground"
                  : "border-border/60 bg-background text-muted-foreground hover:border-border hover:bg-white/[0.03] hover:text-foreground"
              )}
              aria-pressed={isSelected}
              title={`${project.name} — ${descriptor.label}`}
            >
              <span
                className={cn("h-1.5 w-1.5 shrink-0 rounded-full", toneDot[descriptor.tone])}
                aria-hidden
              />
              <span className="truncate max-w-[12rem]">{project.name}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
