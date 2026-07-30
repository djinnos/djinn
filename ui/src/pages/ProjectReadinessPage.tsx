import { useCallback, useEffect, useRef, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { ProjectReadinessPanel } from "@/components/projects/ProjectReadinessPanel";
import { Button } from "@/components/ui/button";
import { useAuthUser } from "@/components/AuthGate";
import { useProjects } from "@/stores/useProjectStore";
import {
  createReadinessKickoffAction,
  fetchActiveOrLatestReadiness,
  fetchReadinessRunDetail,
  ReadinessHttpError,
  type ReadinessKickoffAction,
  type ReadinessRunDetail,
} from "@/api/readiness";

const POLL_INTERVAL_MS = 5_000;
const TERMINAL_STATUSES = new Set([
  "completed",
  "completed_with_errors",
  "failed",
]);

function errorMessage(error: unknown): string {
  if (error instanceof ReadinessHttpError) {
    return `${error.kind}${error.code ? ` (${error.code})` : ""}: ${error.message}`;
  }
  return error instanceof Error
    ? error.message
    : "Unexpected readiness request failure";
}

export function ProjectReadinessPage() {
  const { id: projectId } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const projects = useProjects();
  const user = useAuthUser();
  const kickoffAction = useRef<ReadinessKickoffAction | null>(null);
  const [detail, setDetail] = useState<ReadinessRunDetail | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshError, setRefreshError] = useState<string | null>(null);
  const [startError, setStartError] = useState<string | null>(null);
  const [startNotice, setStartNotice] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  const load = useCallback(async () => {
    if (!projectId) return;
    setRefreshError(null);
    try {
      const summary = await fetchActiveOrLatestReadiness(projectId);
      setDetail(
        summary ? await fetchReadinessRunDetail(projectId, summary.id) : null,
      );
    } catch (error) {
      setRefreshError(errorMessage(error));
    } finally {
      setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    if (!detail || TERMINAL_STATUSES.has(detail.run.status)) return;
    const interval = window.setInterval(() => void load(), POLL_INTERVAL_MS);
    return () => window.clearInterval(interval);
  }, [detail, load]);

  const start = async () => {
    if (!projectId || starting) return;
    const action = kickoffAction.current ?? createReadinessKickoffAction();
    kickoffAction.current = action;
    setStarting(true);
    setStartError(null);
    setStartNotice(null);
    try {
      const response = await action.kickoff(projectId);
      if (response.reused || !response.created) {
        setStartNotice(
          "A readiness analysis is already running; showing the existing run.",
        );
      }
      await load();
    } catch (error) {
      setStartError(errorMessage(error));
    } finally {
      kickoffAction.current = null;
      setStarting(false);
    }
  };

  if (!projectId)
    return (
      <main className="p-6">
        <h1>Project readiness</h1>
        <p role="alert">A project ID is required.</p>
      </main>
    );

  const project = projects.find((candidate) => candidate.id === projectId);
  const ownerContext = user
    ? `${user.name ?? user.login} (@${user.login})`
    : "Authenticated owner unavailable";
  return (
    <main className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between border-b px-6 py-4">
        <div>
          <h1 className="text-lg font-semibold">Project readiness</h1>
          <p className="text-sm text-muted-foreground">
            {project?.name ?? projectId} · Authenticated owner: {ownerContext}
          </p>
        </div>
        <div className="flex gap-2">
          <Button variant="outline" onClick={() => navigate("/repositories")}>
            Back
          </Button>
          <Button
            variant="outline"
            disabled={loading || starting}
            onClick={() => void load()}
          >
            Refresh
          </Button>
        </div>
      </header>
      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto max-w-5xl space-y-4">
          {loading ? <p role="status">Loading readiness analysis…</p> : null}
          {refreshError ? (
            <div role="alert">
              Could not load readiness: {refreshError}{" "}
              <Button variant="link" onClick={() => void load()}>
                Retry
              </Button>
            </div>
          ) : null}
          {startNotice ? <p role="status">{startNotice}</p> : null}
          {!loading && (
            <ProjectReadinessPanel
              detail={detail}
              ownerContext={ownerContext}
              isStarting={starting}
              startError={startError}
              onStart={() => void start()}
            />
          )}
        </div>
      </div>
    </main>
  );
}
