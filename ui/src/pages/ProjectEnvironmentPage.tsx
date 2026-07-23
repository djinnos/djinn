/**
 * ProjectEnvironmentPage — `/projects/:id/environment`.
 *
 * A project's environment is driven by a CATALOG IMAGE (languages /
 * system_packages / env all live on the image now), not per-project config.
 * This page therefore exposes:
 *   1. A catalog-image picker (the primary control, in the header).
 *   2. A Code-graph workspaces editor (per-project SCIP path→language map),
 *      persisted into `environment_config.workspaces`.
 *
 * The old per-project Form / Raw JSON language+package editors are gone.
 */
import { useCallback, useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  ArrowLeft02Icon,
  Delete02Icon,
  Loading02Icon,
  FloppyDiskIcon,
  PlusSignIcon,
  RefreshIcon,
} from "@hugeicons/core-free-icons";

import { EmptyState } from "@/components/EmptyState";
import { PageHeader } from "@/components/PageHeader";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Skeleton } from "@/components/ui/skeleton";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  type EnvironmentConfig,
  type Workspace,
  LANGUAGE_KEYS,
  fetchEnvironmentConfig,
  saveEnvironmentConfig,
} from "@/api/environmentConfig";
import { useProjects } from "@/stores/useProjectStore";
import { showToast } from "@/lib/toast";
import { ProjectImagePicker } from "@/components/images/ProjectImagePicker";
import { useAuthUser } from "@/components/AuthGate";

const LANGUAGE_LABELS: Record<string, string> = {
  rust: "Rust",
  node: "Node",
  python: "Python",
  go: "Go",
  java: "Java",
  ruby: "Ruby",
  dotnet: ".NET",
  clang: "Clang",
};

export function ProjectEnvironmentPage() {
  const { id: projectId } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const projects = useProjects();
  const project = projects.find((p) => p.id === projectId);
  // Image assignment is an org-blast-radius setting; non-admins (and a null
  // user) view it read-only.
  const isAdmin = useAuthUser()?.isAdmin ?? false;

  const [config, setConfig] = useState<EnvironmentConfig | null>(null);
  const [workspaces, setWorkspaces] = useState<Workspace[]>([]);
  const [selectedImageId, setSelectedImageId] = useState<string | null>(null);
  const [selectedImageName, setSelectedImageName] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);

  const load = useCallback(async () => {
    if (!projectId) return;
    setLoading(true);
    try {
      const { config: fetched, selectedImageId: imageId, selectedImageName: imageName } =
        await fetchEnvironmentConfig(projectId);
      setConfig(fetched);
      setWorkspaces(fetched.workspaces);
      setSelectedImageId(imageId);
      setSelectedImageName(imageName);
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to load environment config";
      showToast.error("Could not load environment config", { description: message });
    } finally {
      setLoading(false);
    }
  }, [projectId]);

  useEffect(() => {
    void load();
  }, [load]);

  const handleSaveWorkspaces = async () => {
    if (!projectId || !config) return;
    // Send the existing config with only `workspaces` changed.
    const toSave: EnvironmentConfig = { ...config, workspaces };
    setSaving(true);
    try {
      const response = await saveEnvironmentConfig(projectId, toSave);
      if (!response.ok) {
        showToast.error("Save failed", { description: response.error });
        return;
      }
      showToast.success("Workspaces saved");
      await load();
    } catch (err) {
      const message = err instanceof Error ? err.message : "Save failed";
      showToast.error("Save failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  if (!projectId) {
    return (
      <EmptyState
        title="No project selected"
        message="A project ID is required in the URL to view its environment settings."
      />
    );
  }

  if (loading || !config) {
    return (
      <div className="flex h-full flex-col overflow-hidden">
        <PageHeader
          title="Environment"
          subtitle="A project's runtime image comes from a shared catalog image. Code-graph workspaces stay per-project."
          leading={
            <Button variant="ghost" size="sm" className="h-8 gap-1.5 px-2 text-xs" disabled>
              <HugeiconsIcon icon={ArrowLeft02Icon} size={14} />
              Back
            </Button>
          }
          actions={
            <div className="flex items-center gap-3">
              <Skeleton className="h-8 w-36" />
              <Skeleton className="h-8 w-20" />
              <Skeleton className="h-8 w-16" />
            </div>
          }
          className="shrink-0 border-b px-6 py-4"
        />
        <div className="flex-1 overflow-y-auto px-6 py-6">
          <div className="mx-auto flex max-w-4xl flex-col gap-4">
            <div className="flex flex-col gap-2">
              <Skeleton className="h-5 w-56" />
              <Skeleton className="h-3 w-80" />
            </div>
            {Array.from({ length: 2 }).map((_, i) => (
              <div
                key={i}
                className="flex items-end gap-2 rounded-lg border bg-background/30 px-4 py-3"
              >
                <div className="flex flex-1 flex-col gap-1.5">
                  <Skeleton className="h-3 w-10" />
                  <Skeleton className="h-9 w-full" />
                </div>
                <div className="flex w-44 flex-col gap-1.5">
                  <Skeleton className="h-3 w-14" />
                  <Skeleton className="h-9 w-full" />
                </div>
                <Skeleton className="h-9 w-9 rounded" />
              </div>
            ))}
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <PageHeader
        title="Environment"
        subtitle={
          <>
            {project?.name ? (
              <span className="text-muted-foreground">· {project.name} · </span>
            ) : null}
            A project&apos;s runtime image comes from a shared catalog image. Code-graph workspaces
            stay per-project.
          </>
        }
        leading={
          <Button
            variant="ghost"
            size="sm"
            className="h-8 gap-1.5 px-2 text-xs"
            onClick={() => navigate("/repositories")}
          >
            <HugeiconsIcon icon={ArrowLeft02Icon} size={14} />
            Back
          </Button>
        }
        actions={
          <div className="flex items-center gap-3">
            <ProjectImagePicker
              projectId={projectId}
              initialImageId={selectedImageId}
              initialImageName={selectedImageName}
              readOnly={!isAdmin}
            />
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                className="h-8 gap-1.5 text-xs"
                onClick={() => void load()}
                disabled={saving}
              >
                <HugeiconsIcon icon={RefreshIcon} size={14} />
                Reload
              </Button>
              <Button
                size="sm"
                className="h-8 gap-1.5 text-xs"
                onClick={() => void handleSaveWorkspaces()}
                disabled={saving}
              >
                {saving ? (
                  <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
                ) : (
                  <HugeiconsIcon icon={FloppyDiskIcon} size={14} />
                )}
                {saving ? "Saving…" : "Save"}
              </Button>
            </div>
          </div>
        }
        className="shrink-0 border-b px-6 py-4"
      />

      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto flex max-w-4xl flex-col gap-4">
          <WorkspacesEditor workspaces={workspaces} onChange={setWorkspaces} />
        </div>
      </div>
    </div>
  );
}

// ── Workspaces editor ─────────────────────────────────────────────────────

function WorkspacesEditor({
  workspaces,
  onChange,
}: {
  workspaces: Workspace[];
  onChange: (next: Workspace[]) => void;
}) {
  const updateAt = (idx: number, patch: Partial<Workspace>) => {
    const next = workspaces.slice();
    next[idx] = { ...next[idx], ...patch };
    onChange(next);
  };
  const remove = (idx: number) => {
    const next = workspaces.slice();
    next.splice(idx, 1);
    onChange(next);
  };
  const add = () => {
    onChange([...workspaces, { root: "", language: "rust" }]);
  };

  return (
    <section className="flex flex-col gap-3">
      <div>
        <h3 className="text-sm font-semibold">Code-graph workspaces</h3>
        <p className="text-xs text-muted-foreground">
          Map project directories to their primary language so the code-graph indexer targets the
          right SCIP toolchain. Leave empty to default to the repo root.
        </p>
      </div>
      <div className="flex flex-col gap-2">
        {workspaces.length === 0 && (
          <div className="rounded-lg border border-dashed bg-background/30 px-4 py-6 text-center">
            <p className="text-xs text-muted-foreground">
              No workspaces configured. Add one below to tell the indexer which directory uses which
              language.
            </p>
          </div>
        )}
        {workspaces.map((ws, idx) => (
          <div
            key={idx}
            className="flex items-end gap-2 rounded-lg border bg-card/50 px-4 py-3 shadow-sm"
          >
            <div className="flex flex-1 flex-col gap-1">
              <Label className="text-xs text-muted-foreground">
                Root{" "}
                <span className="font-normal text-muted-foreground/70">(relative to repo root)</span>
              </Label>
              <Input
                value={ws.root}
                onChange={(e) => updateAt(idx, { root: e.target.value })}
                placeholder="e.g. server/ — leave empty for repo root"
                className="font-mono text-xs"
              />
            </div>
            <div className="flex w-44 flex-col gap-1">
              <Label className="text-xs text-muted-foreground">
                Language{" "}
                <span className="font-normal text-muted-foreground/70">(indexer)</span>
              </Label>
              <Select
                value={ws.language}
                onValueChange={(v) => typeof v === "string" && updateAt(idx, { language: v })}
              >
                <SelectTrigger className="w-full">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {LANGUAGE_KEYS.map((lang) => (
                    <SelectItem key={lang} value={lang}>
                      {LANGUAGE_LABELS[lang] ?? lang}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <Button
              type="button"
              variant="ghost"
              size="sm"
              className="h-9 w-9 p-0 text-muted-foreground hover:text-red-400"
              onClick={() => remove(idx)}
            >
              <HugeiconsIcon icon={Delete02Icon} size={14} />
            </Button>
          </div>
        ))}
        <Button
          type="button"
          variant="outline"
          size="sm"
          className="w-fit gap-1.5 text-xs"
          onClick={add}
        >
          <HugeiconsIcon icon={PlusSignIcon} size={12} />
          Add workspace
        </Button>
      </div>
    </section>
  );
}
