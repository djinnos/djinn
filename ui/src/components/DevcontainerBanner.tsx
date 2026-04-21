import { useCallback, useEffect, useMemo, useState } from "react";
import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import {
  Alert02Icon,
  Cancel01Icon,
  Copy01Icon,
  Tick02Icon,
  Loading02Icon,
  RefreshIcon,
  FileValidationIcon,
  LinkSquare02Icon,
  GitPullRequestIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  fetchDevcontainerStatus,
  openDevcontainerPr,
  retriggerImageBuild,
  type DevcontainerPrRef,
  type DevcontainerStatus,
} from "@/api/devcontainer";
import { useClipboard } from "@/hooks/useClipboard";
import { showToast } from "@/lib/toast";
import { cn } from "@/lib/utils";

interface DevcontainerBannerProps {
  projectId: string;
  projectName?: string;
}

type BannerState =
  | { kind: "missing"; starter: string | null; status: DevcontainerStatus }
  | { kind: "missing_lock"; status: DevcontainerStatus }
  | { kind: "building"; status: DevcontainerStatus }
  | { kind: "failed"; status: DevcontainerStatus }
  | { kind: "ready" }
  | { kind: "unknown" };

function deriveState(status: DevcontainerStatus | null): BannerState {
  if (!status) return { kind: "unknown" };
  if (!status.has_devcontainer) {
    return {
      kind: "missing",
      starter: status.starter_json ?? null,
      status,
    };
  }
  if (!status.has_devcontainer_lock) {
    return { kind: "missing_lock", status };
  }
  if (status.image_status === "building") {
    return { kind: "building", status };
  }
  if (status.image_status === "failed") {
    return { kind: "failed", status };
  }
  // ready / none-with-devcontainer: nothing to surface.
  return { kind: "ready" };
}

export function DevcontainerBanner({ projectId, projectName }: DevcontainerBannerProps) {
  const [status, setStatus] = useState<DevcontainerStatus | null>(null);
  const [dismissed, setDismissed] = useState(false);
  const [rebuilding, setRebuilding] = useState(false);
  const [openingPr, setOpeningPr] = useState(false);
  const { copy, copied } = useClipboard();

  const refresh = useCallback(async () => {
    try {
      const next = await fetchDevcontainerStatus(projectId);
      setStatus(next);
    } catch (err) {
      // Silent — banner simply won't render; don't spam toasts on boot.
      console.warn("devcontainer status fetch failed", err);
    }
  }, [projectId]);

  useEffect(() => {
    let active = true;
    void (async () => {
      try {
        const next = await fetchDevcontainerStatus(projectId);
        if (active) setStatus(next);
      } catch (err) {
        console.warn("devcontainer status fetch failed", err);
      }
    })();
    return () => {
      active = false;
    };
  }, [projectId]);

  // Reset dismissal when project changes.
  useEffect(() => setDismissed(false), [projectId]);

  const state = useMemo(() => deriveState(status), [status]);

  const handleOpenPr = useCallback(async () => {
    setOpeningPr(true);
    try {
      const response = await openDevcontainerPr(projectId);
      if (response.error || !response.pr) {
        showToast.error("Could not open devcontainer PR", {
          description: response.error ?? undefined,
        });
        return;
      }
      showToast.success(
        response.already_open ? "PR already open" : "PR opened",
        { description: response.pr.url },
      );
      window.open(response.pr.url, "_blank", "noopener,noreferrer");
      await refresh();
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to open PR";
      showToast.error("Could not open devcontainer PR", { description: message });
    } finally {
      setOpeningPr(false);
    }
  }, [projectId, refresh]);

  const handleRebuild = useCallback(async () => {
    setRebuilding(true);
    try {
      const response = await retriggerImageBuild(projectId);
      if (response.status === "ok") {
        showToast.success("Rebuild enqueued");
        await refresh();
      } else {
        showToast.error("Could not enqueue rebuild", {
          description: response.error ?? undefined,
        });
      }
    } catch (err) {
      const message = err instanceof Error ? err.message : "Failed to enqueue rebuild";
      showToast.error("Could not enqueue rebuild", { description: message });
    } finally {
      setRebuilding(false);
    }
  }, [projectId, refresh]);

  if (dismissed) return null;
  if (state.kind === "ready" || state.kind === "unknown") return null;

  const label = projectName ? ` for ${projectName}` : "";
  const isError = state.kind === "failed";
  const isInfo = state.kind === "building";
  const ring = isError
    ? "ring-red-500/50 bg-red-500/10"
    : isInfo
    ? "ring-sky-500/50 bg-sky-500/10"
    : "ring-amber-500/50 bg-amber-500/10";
  const iconBg = isError
    ? "bg-red-500/20"
    : isInfo
    ? "bg-sky-500/20"
    : "bg-amber-500/20";
  const iconText = isError
    ? "text-red-400"
    : isInfo
    ? "text-sky-400"
    : "text-amber-300";
  const titleColor = isError
    ? "text-red-200"
    : isInfo
    ? "text-sky-200"
    : "text-amber-200";

  const title =
    state.kind === "missing"
      ? `Set up your devcontainer${label}`
      : state.kind === "missing_lock"
      ? `Missing devcontainer-lock.json${label}`
      : state.kind === "building"
      ? `Building project image${label}`
      : `Image build failed${label}`;

  const description =
    state.kind === "missing"
      ? "Djinn needs a devcontainer.json committed to your repo before it can run tasks. A starter is generated below from the detected stack — copy it into .devcontainer/devcontainer.json, commit, and push."
      : state.kind === "missing_lock"
      ? "Commit a devcontainer-lock.json so feature versions are pinned and builds stay reproducible."
      : state.kind === "building"
      ? "The per-project image is being built in the cluster. This usually takes 1–3 minutes on first build; cached layers make subsequent rebuilds much faster."
      : state.status.image_last_error
      ? `The last build failed: ${state.status.image_last_error}`
      : "The last build failed.";

  return (
    <Card className={cn("mx-4 border-none ring-1", ring)}>
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className={cn("flex h-8 w-8 shrink-0 items-center justify-center rounded-full", iconBg)}>
              {state.kind === "building" ? (
                <HugeiconsIcon
                  icon={Loading02Icon}
                  className={cn("size-4 animate-spin", iconText)}
                />
              ) : (
                <HugeiconsIcon
                  icon={isError ? Alert02Icon : FileValidationIcon}
                  className={cn("size-4", iconText)}
                />
              )}
            </div>
            <div className="flex flex-col gap-1">
              <h3 className={cn("text-sm font-semibold", titleColor)}>{title}</h3>
              <p className="text-sm text-muted-foreground">{description}</p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Dismiss devcontainer banner"
            onClick={() => setDismissed(true)}
            className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
          </button>
        </div>

        {state.kind === "missing" && state.starter && (
          <div className="mt-3 pl-11">
            <div className="flex items-center justify-between">
              <span className="text-xs font-medium text-muted-foreground">
                Generated .devcontainer/devcontainer.json
              </span>
              <Button
                variant="ghost"
                size="sm"
                className="h-7 gap-1.5 px-2 text-xs"
                onClick={() => void copy(state.starter ?? "")}
                disabled={!state.starter}
              >
                <HugeiconsIcon icon={copied ? Tick02Icon : Copy01Icon} size={14} />
                {copied ? "Copied" : "Copy"}
              </Button>
            </div>
            <pre className="mt-1 max-h-64 overflow-auto rounded-md border border-border/50 bg-black/30 p-3 text-xs leading-relaxed text-muted-foreground">
              <code>{state.starter}</code>
            </pre>
            <PrButton
              existingPr={state.status.open_setup_pr ?? null}
              opening={openingPr}
              onOpen={() => void handleOpenPr()}
            />
          </div>
        )}

        {state.kind === "missing_lock" && (
          <div className="mt-3 pl-11">
            <p className="text-xs text-muted-foreground">Run this in your repo root:</p>
            <pre className="mt-1 rounded-md border border-border/50 bg-black/30 p-3 text-xs text-muted-foreground">
              <code>devcontainer features info lock --workspace-folder .</code>
            </pre>
          </div>
        )}

        {(state.kind === "failed" || state.kind === "building") && (
          <div className="mt-3 flex flex-wrap items-center gap-2 pl-11">
            {state.kind === "failed" && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 gap-1.5 px-3 text-xs"
                onClick={() => void handleRebuild()}
                disabled={rebuilding}
              >
                {rebuilding ? (
                  <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
                ) : (
                  <HugeiconsIcon icon={RefreshIcon} size={14} />
                )}
                Rebuild
              </Button>
            )}
            {state.kind === "building" && (
              <Button
                variant="ghost"
                size="sm"
                className="h-7 gap-1.5 px-3 text-xs"
                onClick={() => void refresh()}
              >
                <HugeiconsIcon icon={RefreshIcon} size={14} />
                Refresh
              </Button>
            )}
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function PrButton({
  existingPr,
  opening,
  onOpen,
}: {
  existingPr: DevcontainerPrRef | null;
  opening: boolean;
  onOpen: () => void;
}) {
  if (existingPr) {
    return (
      <a
        href={existingPr.url}
        target="_blank"
        rel="noopener noreferrer"
        className="mt-3 inline-flex h-8 items-center gap-1.5 rounded-md bg-amber-500/15 px-3 text-xs font-medium text-amber-200 ring-1 ring-amber-500/40 transition-colors hover:bg-amber-500/25"
      >
        <HugeiconsIcon icon={GitPullRequestIcon} size={14} />
        View PR #{existingPr.number}
        <HugeiconsIcon icon={LinkSquare02Icon} size={12} />
      </a>
    );
  }

  return (
    <Button
      variant="default"
      size="sm"
      className="mt-3 h-8 gap-1.5 px-3 text-xs"
      onClick={onOpen}
      disabled={opening}
    >
      {opening ? (
        <HugeiconsIcon icon={Loading02Icon} size={14} className="animate-spin" />
      ) : (
        <HugeiconsIcon icon={GitPullRequestIcon} size={14} />
      )}
      {opening ? "Opening PR…" : "Open PR"}
    </Button>
  );
}
