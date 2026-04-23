/**
 * ProjectEnvironmentPage — `/projects/:id/environment`.
 *
 * Edits `projects.environment_config` for a single project. Two panes:
 *   1. Form editor — driven by the EnvironmentConfig shape.
 *   2. Raw JSON editor — Monaco-less textarea fallback with parse errors.
 *
 * Saves call `project_environment_config_set`; server-side validation
 * (shell-injection guards, workspace slug dedup) surfaces back through
 * the MCP response.
 */
import { useCallback, useEffect, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  ArrowLeft02Icon,
  Loading02Icon,
  FloppyDiskIcon,
  RefreshIcon,
  FileValidationIcon,
} from "@hugeicons/core-free-icons";

import { Button } from "@/components/ui/button";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Textarea } from "@/components/ui/textarea";
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
  AlertDialogTrigger,
} from "@/components/ui/alert-dialog";
import {
  type EnvironmentConfig,
  fetchEnvironmentConfig,
  normalizeConfig,
  resetEnvironmentConfig,
  saveEnvironmentConfig,
} from "@/api/environmentConfig";
import { useProjects } from "@/stores/useProjectStore";
import { showToast } from "@/lib/toast";
import { EnvironmentConfigForm } from "@/components/environmentConfig/EnvironmentConfigForm";

export function ProjectEnvironmentPage() {
  const { id: projectId } = useParams<{ id: string }>();
  const navigate = useNavigate();
  const projects = useProjects();
  const project = projects.find((p) => p.id === projectId);

  const [config, setConfig] = useState<EnvironmentConfig | null>(null);
  const [rawText, setRawText] = useState<string>("");
  const [rawError, setRawError] = useState<string | null>(null);
  const [seeded, setSeeded] = useState<boolean>(true);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [mode, setMode] = useState<"form" | "raw">("form");

  const load = useCallback(async () => {
    if (!projectId) return;
    setLoading(true);
    try {
      const { config: fetched, seeded } = await fetchEnvironmentConfig(projectId);
      setConfig(fetched);
      setRawText(JSON.stringify(fetched, null, 2));
      setRawError(null);
      setSeeded(seeded);
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

  // When the user switches panes, sync state in whichever direction makes
  // sense. Form → raw: serialize the current form state. Raw → form:
  // require a valid parse first (we surface the parse error inline and
  // keep the user on the raw pane via the `disabled` flag below).
  const handleModeChange = (next: string) => {
    if (next === mode) return;
    if (next === "raw" && config) {
      setRawText(JSON.stringify(config, null, 2));
      setRawError(null);
    }
    if (next === "form") {
      const parsed = tryParseRaw(rawText);
      if (parsed.ok) {
        setConfig(normalizeConfig(parsed.value));
        setRawError(null);
      } else {
        setRawError(parsed.error);
        return;
      }
    }
    setMode(next as "form" | "raw");
  };

  const handleFormChange = useCallback((next: EnvironmentConfig) => {
    setConfig(next);
    setRawText(JSON.stringify(next, null, 2));
  }, []);

  const handleRawChange = (next: string) => {
    setRawText(next);
    const parsed = tryParseRaw(next);
    if (parsed.ok) {
      setRawError(null);
      setConfig(normalizeConfig(parsed.value));
    } else {
      setRawError(parsed.error);
    }
  };

  const handleSave = async () => {
    if (!projectId || !config) return;
    // If user is in raw mode, make sure what we send is exactly what they
    // see (they may have hand-edited keys the form doesn't know about).
    let toSave: unknown = config;
    if (mode === "raw") {
      const parsed = tryParseRaw(rawText);
      if (!parsed.ok) {
        showToast.error("Cannot save — JSON is invalid", { description: parsed.error });
        return;
      }
      toSave = parsed.value;
    }
    setSaving(true);
    try {
      const response = await saveEnvironmentConfig(projectId, toSave as EnvironmentConfig);
      if (!response.ok) {
        showToast.error("Save failed", { description: response.error });
        return;
      }
      showToast.success("Environment config saved", {
        description: "Image will rebuild on the next mirror-fetch tick.",
      });
      await load();
    } catch (err) {
      const message = err instanceof Error ? err.message : "Save failed";
      showToast.error("Save failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  const handleResetFromDetection = async () => {
    if (!projectId) return;
    setSaving(true);
    try {
      const result = await resetEnvironmentConfig(projectId);
      if (!result.ok || !result.config) {
        showToast.error("Reset failed", { description: result.error });
        return;
      }
      setConfig(result.config);
      setRawText(JSON.stringify(result.config, null, 2));
      setRawError(null);
      setSeeded(true);
      showToast.success("Config reset to auto-detected", {
        description: "Image will rebuild on the next mirror-fetch tick.",
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : "Reset failed";
      showToast.error("Reset failed", { description: message });
    } finally {
      setSaving(false);
    }
  };

  if (!projectId) {
    return <EmptyState message="No project id in URL." />;
  }

  if (loading || !config) {
    return (
      <div className="flex h-full items-center justify-center">
        <HugeiconsIcon icon={Loading02Icon} className="size-5 animate-spin text-muted-foreground" />
      </div>
    );
  }

  return (
    <div className="flex h-full flex-col overflow-hidden">
      <header className="flex items-center justify-between gap-3 border-b px-6 py-4">
        <div className="flex items-center gap-3">
          <Button
            variant="ghost"
            size="sm"
            className="h-8 gap-1.5 px-2 text-xs"
            onClick={() => navigate("/repositories")}
          >
            <HugeiconsIcon icon={ArrowLeft02Icon} size={14} />
            Back
          </Button>
          <div>
            <h1 className="text-lg font-semibold">
              Environment config
              {project?.name ? <span className="text-muted-foreground"> · {project.name}</span> : null}
            </h1>
            <p className="text-xs text-muted-foreground">
              Controls the per-project runtime image + warm/task Pod environment. Saving triggers a rebuild.
            </p>
          </div>
        </div>
        <div className="flex items-center gap-2">
          <ResetFromDetectionButton onConfirm={handleResetFromDetection} />
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
            onClick={() => void handleSave()}
            disabled={saving || (mode === "raw" && rawError !== null)}
          >
            {saving ? (
              <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
            ) : (
              <HugeiconsIcon icon={FloppyDiskIcon} size={14} />
            )}
            {saving ? "Saving…" : "Save"}
          </Button>
        </div>
      </header>

      <div className="flex-1 overflow-y-auto px-6 py-6">
        <div className="mx-auto flex max-w-4xl flex-col gap-4">
          {!seeded && (
            <Banner
              tone="warn"
              title="Config not yet auto-detected"
              description="This project's environment_config is still empty. It will be populated on the next server restart from the detected stack. You can also author the config directly here."
            />
          )}
          {config.source === "auto-detected" && seeded && (
            <Banner
              tone="info"
              title="Auto-detected config"
              description="This config was generated from the detected stack. Saving any change marks it as user-edited and stops future auto-reseeds."
            />
          )}

          <Tabs
            value={mode}
            onValueChange={(v) => typeof v === "string" && handleModeChange(v)}
            className="flex flex-col"
          >
            <TabsList className="w-fit">
              <TabsTrigger value="form">Form</TabsTrigger>
              <TabsTrigger value="raw">Raw JSON</TabsTrigger>
            </TabsList>
            <TabsContent value="form" className="mt-4">
              <EnvironmentConfigForm config={config} onChange={handleFormChange} />
            </TabsContent>
            <TabsContent value="raw" className="mt-4">
              <div className="flex flex-col gap-2">
                <Textarea
                  value={rawText}
                  onChange={(e) => handleRawChange(e.target.value)}
                  className="min-h-[480px] font-mono text-xs"
                  spellCheck={false}
                />
                {rawError ? (
                  <p className="text-xs text-destructive">{rawError}</p>
                ) : (
                  <p className="text-xs text-muted-foreground">
                    JSON is valid. Server-side validation still runs on save.
                  </p>
                )}
              </div>
            </TabsContent>
          </Tabs>
        </div>
      </div>
    </div>
  );
}

// ── Helpers ─────────────────────────────────────────────────────────────

type ParseResult<T> = { ok: true; value: T } | { ok: false; error: string };

function tryParseRaw(text: string): ParseResult<unknown> {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch (err) {
    const message = err instanceof Error ? err.message : "invalid JSON";
    return { ok: false, error: message };
  }
}

function EmptyState({ message }: { message: string }) {
  return (
    <div className="flex h-full items-center justify-center">
      <p className="text-sm text-muted-foreground">{message}</p>
    </div>
  );
}

function Banner({
  tone,
  title,
  description,
}: {
  tone: "info" | "warn";
  title: string;
  description: string;
}) {
  const ring = tone === "warn" ? "ring-amber-500/40 bg-amber-500/5" : "ring-sky-500/40 bg-sky-500/5";
  const iconColor = tone === "warn" ? "text-amber-300" : "text-sky-300";
  const titleColor = tone === "warn" ? "text-amber-200" : "text-sky-200";
  return (
    <div className={`flex items-start gap-3 rounded-lg px-4 py-3 ring-1 ${ring}`}>
      <HugeiconsIcon icon={FileValidationIcon} className={`mt-0.5 size-4 shrink-0 ${iconColor}`} />
      <div className="flex flex-col gap-0.5">
        <span className={`text-sm font-medium ${titleColor}`}>{title}</span>
        <span className="text-xs text-muted-foreground">{description}</span>
      </div>
    </div>
  );
}

function ResetFromDetectionButton({ onConfirm }: { onConfirm: () => void | Promise<void> }) {
  const [open, setOpen] = useState(false);
  return (
    <AlertDialog open={open} onOpenChange={setOpen}>
      <AlertDialogTrigger
        render={
          <Button
            variant="ghost"
            size="sm"
            className="h-8 gap-1.5 text-xs text-muted-foreground"
            type="button"
          >
            Reset from auto-detection
          </Button>
        }
      />
      <AlertDialogContent size="sm">
        <AlertDialogHeader>
          <AlertDialogTitle>Reset to auto-detected config?</AlertDialogTitle>
          <AlertDialogDescription>
            This discards every change made in this editor and repopulates the config from the detected
            stack. Any custom lifecycle hooks, env vars, or verification rules will be lost.
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogCancel>Cancel</AlertDialogCancel>
          <AlertDialogAction
            onClick={() => {
              void onConfirm();
              setOpen(false);
            }}
          >
            Reset
          </AlertDialogAction>
        </AlertDialogFooter>
      </AlertDialogContent>
    </AlertDialog>
  );
}

