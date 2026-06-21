/**
 * CodeGraphPage — top-level shell for `/code-graph`.
 *
 * D1 stood up an empty Sigma canvas and a project picker.
 * D2 swapped the empty canvas for `<CodeGraphCanvas>`, fetching the
 *   `code_graph snapshot` payload and rendering through Sigma + FA2.
 * D3 layered:
 *   - `<GraphToolbar>`         (filters, lenses, and DOI focus controls)
 *   - `<SymbolDetailPanel>`    (right rail; opens on selection)
 *   - `<QueryPalette>`         (Cmd-K fuzzy hybrid search)
 *
 * The store survives across the canvas remount on project change —
 * the canvas itself calls `reset()` on mount so stale highlights
 * don't leak between projects.
 */

import { useEffect, useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { ConnectIcon } from "@hugeicons/core-free-icons";

import { fetchWorkspaces, type CodeGraphWorkspace } from "@/api/codeGraph";
import { CodeGraphCanvas } from "@/components/codegraph/CodeGraphCanvas";
import { GraphToolbar } from "@/components/codegraph/GraphToolbar";
import { QueryFAB } from "@/components/codegraph/QueryFAB";
import { QueryPalette } from "@/components/codegraph/QueryPalette";
import { SymbolDetailPanel } from "@/components/codegraph/SymbolDetailPanel";
import { useCodeGraphStore } from "@/stores/codeGraphStore";
import {
  useProjectStore,
  useProjects,
  useSelectedProject,
  useSelectedProjectId,
} from "@/stores/useProjectStore";
import { cn } from "@/lib/utils";

type WorkspaceFetchState = {
  projectId: string | null;
  status: "idle" | "loading" | "success" | "error";
  workspaces: CodeGraphWorkspace[];
  error: string | null;
};

const INITIAL_WORKSPACE_STATE: WorkspaceFetchState = {
  projectId: null,
  status: "idle",
  workspaces: [],
  error: null,
};

function WorkspaceSelector({
  status,
  workspaces,
  error,
}: Pick<WorkspaceFetchState, "status" | "workspaces" | "error">) {
  const selectedWorkspaceSlug = useCodeGraphStore(
    (state) => state.selectedWorkspaceSlug,
  );
  const setSelectedWorkspaceSlug = useCodeGraphStore(
    (state) => state.setSelectedWorkspaceSlug,
  );

  if (status === "loading") {
    return (
      <span className="text-xs text-muted-foreground/70" role="status">
        Loading workspaces…
      </span>
    );
  }

  if (status === "error") {
    return (
      <span className="text-xs text-muted-foreground/70" role="status">
        {error ?? "Workspace list unavailable."}
      </span>
    );
  }

  if (workspaces.length <= 1) {
    return null;
  }

  return (
    <div className="flex shrink-0 items-center gap-2">
      <label
        htmlFor="code-graph-workspace"
        className="shrink-0 text-xs uppercase tracking-wide text-muted-foreground/70"
      >
        Workspace
      </label>
      <select
        id="code-graph-workspace"
        className="rounded-md border border-border/60 bg-background px-2 py-1 text-sm text-foreground"
        value={selectedWorkspaceSlug ?? ""}
        onChange={(e) => setSelectedWorkspaceSlug(e.target.value || null)}
        aria-label="Select workspace"
      >
        <option value="">All</option>
        {workspaces.map((workspace) => (
          <option key={workspace.slug} value={workspace.slug}>
            {workspace.display ?? workspace.slug}
          </option>
        ))}
      </select>
    </div>
  );
}

function ProjectPicker({
  workspaceState,
}: {
  workspaceState: WorkspaceFetchState;
}) {
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
      {selectedProjectId && workspaceState.projectId === selectedProjectId ? (
        <WorkspaceSelector
          status={workspaceState.status}
          workspaces={workspaceState.workspaces}
          error={workspaceState.error}
        />
      ) : null}
      <span className="ml-auto text-[10px] uppercase tracking-wide text-muted-foreground/60">
        Press{" "}
        <kbd className="rounded border border-border/60 bg-background px-1 py-0.5 font-mono text-[10px]">
          ⌘K
        </kbd>{" "}
        to search
      </span>
    </div>
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
  const selectedWorkspaceSlug = useCodeGraphStore(
    (state) => state.selectedWorkspaceSlug,
  );
  const setSelectedWorkspaceSlug = useCodeGraphStore(
    (state) => state.setSelectedWorkspaceSlug,
  );
  const [workspaceState, setWorkspaceState] = useState<WorkspaceFetchState>(
    INITIAL_WORKSPACE_STATE,
  );

  useEffect(() => {
    setSelectedWorkspaceSlug(null);

    if (!selectedProjectId) {
      setWorkspaceState(INITIAL_WORKSPACE_STATE);
      return;
    }

    let cancelled = false;
    setWorkspaceState({
      projectId: selectedProjectId,
      status: "loading",
      workspaces: [],
      error: null,
    });

    void fetchWorkspaces(selectedProjectId)
      .then((workspaces) => {
        if (cancelled) return;
        setWorkspaceState({
          projectId: selectedProjectId,
          status: "success",
          workspaces,
          error: null,
        });
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setWorkspaceState({
          projectId: selectedProjectId,
          status: "error",
          workspaces: [],
          error:
            error instanceof Error
              ? error.message
              : "Workspace list unavailable.",
        });
      });

    return () => {
      cancelled = true;
    };
  }, [selectedProjectId, setSelectedWorkspaceSlug]);

  useEffect(() => {
    if (
      workspaceState.status === "success" &&
      selectedWorkspaceSlug &&
      !workspaceState.workspaces.some(
        (workspace) => workspace.slug === selectedWorkspaceSlug,
      )
    ) {
      setSelectedWorkspaceSlug(null);
    }
  }, [selectedWorkspaceSlug, setSelectedWorkspaceSlug, workspaceState]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ProjectPicker workspaceState={workspaceState} />
      {project && selectedProjectId && <GraphToolbar />}
      <div className={cn("relative flex min-h-0 flex-1")}>
        {project && selectedProjectId ? (
          <>
            <div className="relative min-w-0 flex-1">
              {/*
                The `key` forces a fresh canvas + fetch when the project
                changes. The hook contract treats remount as the canonical
                "reset" path — the canvas also calls `reset()` on mount so
                cross-project highlight leaks are impossible.
              */}
              <CodeGraphCanvas
                key={selectedProjectId}
                projectId={selectedProjectId}
              />
            </div>
            <SymbolDetailPanel projectId={selectedProjectId} />
            <QueryPalette projectId={selectedProjectId} />
            <QueryFAB projectId={selectedProjectId} />
          </>
        ) : (
          <EmptyHint message="Select a project to view its code graph." />
        )}
      </div>
    </div>
  );
}
