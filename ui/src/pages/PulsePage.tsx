import { useQuery } from "@tanstack/react-query";
import { HugeiconsIcon } from "@hugeicons/react";
import { Pulse01Icon } from "@hugeicons/core-free-icons";
import { useSelectedProject } from "@/stores/useProjectStore";
import { callMcpTool } from "@/api/mcpClient";
import { fetchDevcontainerStatus } from "@/api/devcontainer";
import { FreshnessStrip } from "@/components/pulse/FreshnessStrip";
import { ArchitectProposalsSection } from "@/components/pulse/ArchitectProposalsSection";
import { HotspotsPanel } from "@/components/pulse/HotspotsPanel";
import { DeadCodePanel } from "@/components/pulse/DeadCodePanel";
import { CyclesPanel } from "@/components/pulse/CyclesPanel";
import { BlastRadiusPanel } from "@/components/pulse/BlastRadiusPanel";
import { AskArchitectDialog } from "@/components/pulse/AskArchitectDialog";
import { PulseSettingsSheet } from "@/components/pulse/PulseSettingsSheet";
import { useArchitectActive } from "@/hooks/useArchitectActive";
import { usePulseSettings } from "@/hooks/usePulseSettings";
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

function NotWarmedState({ reason }: { reason?: string }) {
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
          {reason ??
            "The canonical code graph hasn’t been warmed yet. Pulse will populate on the next Planner patrol or Architect spike."}
        </p>
      </div>
    </div>
  );
}

function WarmingState({ title, description }: { title?: string; description?: string }) {
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
          {title ?? "Architect is patrolling your codebase…"}
        </h2>
        <p className="mt-2 text-sm text-muted-foreground">
          {description ??
            "Reading symbols, computing centrality, mapping dependencies. This usually takes ~30 seconds."}
        </p>
      </div>
    </div>
  );
}

export function PulsePage() {
  const project = useSelectedProject();
  const projectPath = project?.path ?? null;
  const projectId = project?.id ?? null;
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

  // Devcontainer + graph-warm pipeline state. Pulse needs this so it can
  // tell the user whether "not warmed" means "waiting for image" vs. "warm
  // Job is running" vs. "project just hasn't had an architect session yet".
  const { data: devcontainer } = useQuery({
    queryKey: ["pulse", "devcontainer-status", projectId],
    queryFn: () => fetchDevcontainerStatus(projectId!),
    enabled: !!projectId,
    staleTime: 15_000,
    refetchInterval: 15_000,
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

  if (!warmed) {
    // Surface the graph-warm pipeline explicitly so the user sees whether
    // Pulse is blocked on the image build, the first warm Job, or just on
    // an architect session.
    if (devcontainer) {
      if (!devcontainer.has_devcontainer) {
        return (
          <NotWarmedState reason="This project has no devcontainer.json yet. Commit one (use the banner on the Repositories page) and Pulse will warm automatically." />
        );
      }
      if (devcontainer.image_status === "failed") {
        return (
          <NotWarmedState reason="The devcontainer image build failed. Fix it from the Repositories banner and Pulse will warm once the rebuild lands." />
        );
      }
      if (devcontainer.image_status === "building") {
        return (
          <WarmingState
            title="Building project image…"
            description="Pulse will warm automatically as soon as the per-project devcontainer image lands."
          />
        );
      }
      if (devcontainer.graph_warm_status === "running") {
        return (
          <WarmingState
            title="Warming code graph…"
            description="Djinn is indexing the project inside its devcontainer image. This usually takes 1–3 minutes on first warm."
          />
        );
      }
    }

    if (architectActive) {
      return <WarmingState />;
    }

    return <NotWarmedState />;
  }

  return (
    <ReadyState
      projectPath={projectPath!}
      lastWarmAt={data?.last_warm_at ?? null}
      pinnedCommit={data?.pinned_commit ?? null}
      commitsSincePin={data?.commits_since_pin ?? null}
      architectActive={architectActive}
    />
  );
}

function ReadyState({
  projectPath,
  lastWarmAt,
  pinnedCommit,
  commitsSincePin,
  architectActive,
}: {
  projectPath: string;
  lastWarmAt: string | null;
  pinnedCommit: string | null;
  commitsSincePin: number | null;
  architectActive: boolean;
}) {
  const { settings, addOrphanIgnore } = usePulseSettings(projectPath);
  return (
    <div className="flex h-full min-h-0 flex-col gap-4 overflow-y-auto p-4 [&>*]:shrink-0">
      <div className="flex items-start gap-2">
        <div className="flex-1">
          <FreshnessStrip
            lastWarmAt={lastWarmAt}
            pinnedCommit={pinnedCommit}
            commitsSincePin={commitsSincePin}
            architectActive={architectActive}
            actions={<AskArchitectDialog projectPath={projectPath} />}
          />
        </div>
        <PulseSettingsSheet projectPath={projectPath} />
      </div>
      <ArchitectProposalsSection projectPath={projectPath} />
      <HotspotsPanel projectPath={projectPath} excludedPaths={settings.excluded_paths} />
      <DeadCodePanel
        projectPath={projectPath}
        excludedPaths={settings.excluded_paths}
        ignoredFiles={settings.orphan_ignore}
        onIgnoreFile={addOrphanIgnore}
      />
      <CyclesPanel projectPath={projectPath} excludedPaths={settings.excluded_paths} />
      <BlastRadiusPanel projectPath={projectPath} excludedPaths={settings.excluded_paths} />
    </div>
  );
}
