/**
 * CodeGraphPage — PR D1 scaffolding.
 *
 * This is the empty shell for the new `/code-graph` view. It mounts a Sigma
 * instance over an empty graphology graph so D2 can swap in real data
 * without touching the lifecycle wiring. There is intentionally no fetch,
 * no layout worker, no interactions yet — those land in D2-D6.
 */

import { useEffect, useRef } from "react";
import Graph from "graphology";
import Sigma from "sigma";
import { HugeiconsIcon } from "@hugeicons/react";
import { ConnectIcon } from "@hugeicons/core-free-icons";

import {
  useProjectStore,
  useProjects,
  useSelectedProject,
  useSelectedProjectId,
} from "@/stores/useProjectStore";
import { cn } from "@/lib/utils";

function ProjectPicker() {
  const projects = useProjects();
  const selectedProjectId = useSelectedProjectId();
  const setSelectedProjectId = useProjectStore(
    (state) => state.setSelectedProjectId,
  );

  if (projects.length === 0) {
    return (
      <div className="border-b border-border/60 bg-background/40 px-4 py-2.5 text-sm text-muted-foreground">
        No projects yet. Add one from the Repositories page.
      </div>
    );
  }

  return (
    <div className="flex shrink-0 items-center gap-2 overflow-x-auto border-b border-border/60 bg-background/40 px-4 py-2.5">
      <label
        htmlFor="code-graph-project"
        className="shrink-0 text-xs uppercase tracking-wide text-muted-foreground/70"
      >
        Project
      </label>
      <select
        id="code-graph-project"
        className="rounded-md border border-border/60 bg-background px-2 py-1 text-sm text-foreground"
        value={selectedProjectId ?? ""}
        onChange={(e) => setSelectedProjectId(e.target.value || null)}
        aria-label="Select project"
      >
        {projects.map((project) => (
          <option key={project.id} value={project.id}>
            {project.name}
          </option>
        ))}
      </select>
    </div>
  );
}

interface SigmaCanvasProps {
  /** Used as a remount key so the Sigma instance is rebuilt when the
   *  selected project changes. D2 will swap in a graph fetch here. */
  resetKey: string;
}

function SigmaCanvas({ resetKey }: SigmaCanvasProps) {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const sigmaRef = useRef<Sigma | null>(null);

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    // PR D1: empty graph. PR D2 hydrates this from `code_graph snapshot`.
    const graph = new Graph();
    const sigma = new Sigma(graph, container, {
      // Sigma 3 picks WebGL by default; keep the call site explicit so D3's
      // node/edge reducers have a known baseline to extend.
      renderEdgeLabels: false,
    });
    sigmaRef.current = sigma;

    return () => {
      sigma.kill();
      sigmaRef.current = null;
    };
    // `resetKey` is intentionally part of the dep list so changing projects
    // tears down + rebuilds Sigma instead of leaking the old instance.
  }, [resetKey]);

  return (
    <div
      ref={containerRef}
      data-testid="code-graph-canvas"
      className="absolute inset-0 bg-background"
    />
  );
}

function EmptyHint({ message }: { message: string }) {
  return (
    <div className="pointer-events-none absolute inset-0 flex items-center justify-center">
      <div className="max-w-sm rounded-lg border border-border/40 bg-background/80 px-5 py-4 text-center backdrop-blur">
        <span className="mx-auto flex h-10 w-10 items-center justify-center rounded-full bg-muted/30 text-muted-foreground/70">
          <HugeiconsIcon icon={ConnectIcon} className="h-5 w-5" />
        </span>
        <p className="mt-3 text-sm text-muted-foreground">{message}</p>
      </div>
    </div>
  );
}

export function CodeGraphPage() {
  const project = useSelectedProject();
  const selectedProjectId = useSelectedProjectId();

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ProjectPicker />
      <div className={cn("relative min-h-0 flex-1")}>
        {project ? (
          <>
            <SigmaCanvas resetKey={selectedProjectId ?? "none"} />
            <EmptyHint message="Graph rendering lands in PR D2." />
          </>
        ) : (
          <EmptyHint message="Select a project to view its code graph." />
        )}
      </div>
    </div>
  );
}
