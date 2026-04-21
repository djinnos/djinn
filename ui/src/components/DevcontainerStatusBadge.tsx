import { useCallback, useEffect, useMemo, useState } from "react";
import { Button } from "@/components/ui/button";
import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover";
import {
  Alert02Icon,
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

interface DevcontainerStatusBadgeProps {
  projectId: string;
  projectName?: string;
}

type BadgeState =
  | { kind: "missing"; starter: string | null; status: DevcontainerStatus }
  | { kind: "building"; status: DevcontainerStatus }
  | { kind: "failed"; status: DevcontainerStatus }
  | { kind: "warming"; status: DevcontainerStatus }
  | { kind: "ready" }
  | { kind: "unknown" };

// NOTE: `status.has_devcontainer_lock` is intentionally NOT surfaced as a
// badge state. The image-controller pipeline only requires
// `devcontainer.json`; the lock file is a reproducibility nudge (it pins
// feature digests via `devcontainer features info lock`) and not worth
// blocking users on. The flag still comes back in the status response for
// future use.
function deriveState(status: DevcontainerStatus | null): BadgeState {
  if (!status) return { kind: "unknown" };
  if (!status.has_devcontainer) {
    return {
      kind: "missing",
      starter: status.starter_json ?? null,
      status,
    };
  }
  if (status.image_status === "building") {
    return { kind: "building", status };
  }
  if (status.image_status === "failed") {
    return { kind: "failed", status };
  }
  if (status.image_status === "ready" && status.graph_warm_status !== "ready") {
    return { kind: "warming", status };
  }
  return { kind: "ready" };
}

type Tone = "info" | "warn" | "error" | "ok";

interface BadgeDescriptor {
  tone: Tone;
  label: string;
  pulse: boolean;
}

function describeBadge(state: BadgeState): BadgeDescriptor | null {
  switch (state.kind) {
    case "missing":
      return { tone: "warn", label: "Setup needed", pulse: false };
    case "building":
      return { tone: "info", label: "Building", pulse: true };
    case "warming":
      return { tone: "info", label: "Warming", pulse: true };
    case "failed":
      return { tone: "error", label: "Build failed", pulse: false };
    case "ready":
    case "unknown":
      return null;
  }
}

const toneDot: Record<Tone, string> = {
  info: "bg-sky-400",
  warn: "bg-amber-400",
  error: "bg-red-400",
  ok: "bg-emerald-400",
};

const toneText: Record<Tone, string> = {
  info: "text-sky-300",
  warn: "text-amber-300",
  error: "text-red-300",
  ok: "text-emerald-300",
};

const toneRing: Record<Tone, string> = {
  info: "border-sky-500/30 hover:border-sky-500/50",
  warn: "border-amber-500/30 hover:border-amber-500/50",
  error: "border-red-500/30 hover:border-red-500/50",
  ok: "border-emerald-500/30 hover:border-emerald-500/50",
};

export function DevcontainerStatusBadge({
  projectId,
  projectName,
}: DevcontainerStatusBadgeProps) {
  const [status, setStatus] = useState<DevcontainerStatus | null>(null);
  const [rebuilding, setRebuilding] = useState(false);
  const [openingPr, setOpeningPr] = useState(false);
  const { copy, copied } = useClipboard();

  const refresh = useCallback(async () => {
    try {
      const next = await fetchDevcontainerStatus(projectId);
      setStatus(next);
    } catch (err) {
      console.warn("devcontainer status fetch failed", err);
    }
  }, [projectId]);

  useEffect(() => {
    let active = true;
    const fetchOnce = async () => {
      try {
        const next = await fetchDevcontainerStatus(projectId);
        if (active) setStatus(next);
      } catch (err) {
        console.warn("devcontainer status fetch failed", err);
      }
    };
    void fetchOnce();
    // Poll every 30s so the badge catches server-side changes (PR merged
    // → mirror fetch on next 60s tick → stack re-detected) without
    // requiring a page refresh.
    const timer = setInterval(() => void fetchOnce(), 30_000);
    return () => {
      active = false;
      clearInterval(timer);
    };
  }, [projectId]);

  const state = useMemo(() => deriveState(status), [status]);
  const descriptor = describeBadge(state);

  if (!descriptor) return null;

  const handleOpenPr = async () => {
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
  };

  const handleRebuild = async () => {
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
      const message =
        err instanceof Error ? err.message : "Failed to enqueue rebuild";
      showToast.error("Could not enqueue rebuild", { description: message });
    } finally {
      setRebuilding(false);
    }
  };

  return (
    <Popover>
      <PopoverTrigger
        onClick={(e) => e.stopPropagation()}
        className={cn(
          "inline-flex items-center gap-1.5 rounded-full border bg-background/60 px-2.5 py-1 text-xs text-muted-foreground transition-colors hover:bg-white/[0.04] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
          toneRing[descriptor.tone],
        )}
        title={descriptor.label}
      >
        <span
          className={cn(
            "h-1.5 w-1.5 shrink-0 rounded-full",
            toneDot[descriptor.tone],
            descriptor.pulse && "animate-pulse [animation-duration:2s]",
          )}
          aria-hidden
        />
        <span className={cn("leading-none", toneText[descriptor.tone])}>
          {descriptor.label}
        </span>
      </PopoverTrigger>

      <PopoverContent
        className="w-96"
        onClick={(e) => e.stopPropagation()}
      >
        <StatusDetail
          state={state}
          projectName={projectName}
          onRefresh={() => void refresh()}
          onRebuild={() => void handleRebuild()}
          onOpenPr={() => void handleOpenPr()}
          rebuilding={rebuilding}
          openingPr={openingPr}
          starter={state.kind === "missing" ? state.starter : null}
          onCopyStarter={() =>
            state.kind === "missing" && void copy(state.starter ?? "")
          }
          copied={copied}
        />
      </PopoverContent>
    </Popover>
  );
}

interface StatusDetailProps {
  state: BadgeState;
  projectName?: string;
  onRefresh: () => void;
  onRebuild: () => void;
  onOpenPr: () => void;
  rebuilding: boolean;
  openingPr: boolean;
  starter: string | null;
  onCopyStarter: () => void;
  copied: boolean;
}

function StatusDetail({
  state,
  projectName,
  onRefresh,
  onRebuild,
  onOpenPr,
  rebuilding,
  openingPr,
  starter,
  onCopyStarter,
  copied,
}: StatusDetailProps) {
  if (state.kind === "ready" || state.kind === "unknown") return null;

  const label = projectName ? ` for ${projectName}` : "";
  const isError = state.kind === "failed";
  const isInfo = state.kind === "building" || state.kind === "warming";

  const iconBg = isError
    ? "bg-red-500/20"
    : isInfo
    ? "bg-sky-500/20"
    : "bg-amber-500/20";
  const iconText = isError
    ? "text-red-300"
    : isInfo
    ? "text-sky-300"
    : "text-amber-300";
  const titleColor = isError
    ? "text-red-200"
    : isInfo
    ? "text-sky-200"
    : "text-amber-200";

  const title =
    state.kind === "missing"
      ? `Set up your devcontainer${label}`
      : state.kind === "building"
      ? `Building project image${label}`
      : state.kind === "warming"
      ? `Warming code graph${label}`
      : `Image build failed${label}`;

  const description =
    state.kind === "missing"
      ? "Djinn needs a devcontainer.json committed to your repo before it can run tasks. A starter is generated below from the detected stack — copy it into .devcontainer/devcontainer.json, commit, and push."
      : state.kind === "building"
      ? "The per-project image is being built in the cluster. This usually takes 1–3 minutes on first build; cached layers make subsequent rebuilds much faster."
      : state.kind === "warming"
      ? "The image is ready. Djinn is now indexing the project's code graph inside that image — task dispatch stays paused until the first warm completes."
      : state.status.image_last_error
      ? `The last build failed: ${state.status.image_last_error}`
      : "The last build failed.";

  return (
    <div className="flex flex-col gap-3">
      <div className="flex items-start gap-2.5">
        <div
          className={cn(
            "flex h-7 w-7 shrink-0 items-center justify-center rounded-full",
            iconBg,
          )}
        >
          {state.kind === "building" || state.kind === "warming" ? (
            <HugeiconsIcon
              icon={Loading02Icon}
              className={cn("size-3.5 animate-spin", iconText)}
            />
          ) : (
            <HugeiconsIcon
              icon={isError ? Alert02Icon : FileValidationIcon}
              className={cn("size-3.5", iconText)}
            />
          )}
        </div>
        <div className="flex min-w-0 flex-col gap-0.5">
          <h3 className={cn("text-sm font-semibold", titleColor)}>{title}</h3>
          <p className="text-xs text-muted-foreground">{description}</p>
        </div>
      </div>

      {state.kind === "missing" && starter && (
        <div className="flex flex-col gap-1.5">
          <div className="flex items-center justify-between">
            <span className="text-[11px] font-medium text-muted-foreground">
              Generated .devcontainer/devcontainer.json
            </span>
            <Button
              variant="ghost"
              size="sm"
              className="h-6 gap-1 px-1.5 text-[11px]"
              onClick={onCopyStarter}
              disabled={!starter}
            >
              <HugeiconsIcon icon={copied ? Tick02Icon : Copy01Icon} size={12} />
              {copied ? "Copied" : "Copy"}
            </Button>
          </div>
          <pre className="max-h-48 overflow-auto rounded-md border border-border/50 bg-black/30 p-2.5 text-[11px] leading-relaxed text-muted-foreground">
            <code>{starter}</code>
          </pre>
          <div className="flex items-center gap-2">
            <PrButton
              existingPr={state.status.open_setup_pr ?? null}
              opening={openingPr}
              onOpen={onOpenPr}
            />
            <Button
              variant="ghost"
              size="sm"
              className="h-7 gap-1.5 px-2.5 text-xs"
              onClick={onRefresh}
              title="Re-check whether the repo has a devcontainer yet"
            >
              <HugeiconsIcon icon={RefreshIcon} size={12} />
              Check again
            </Button>
          </div>
        </div>
      )}

      {(state.kind === "failed" ||
        state.kind === "building" ||
        state.kind === "warming") && (
        <div className="flex items-center gap-1.5">
          {state.kind === "failed" && (
            <Button
              variant="ghost"
              size="sm"
              className="h-7 gap-1.5 px-2.5 text-xs"
              onClick={onRebuild}
              disabled={rebuilding}
            >
              {rebuilding ? (
                <HugeiconsIcon
                  icon={Loading02Icon}
                  size={12}
                  className="animate-spin"
                />
              ) : (
                <HugeiconsIcon icon={RefreshIcon} size={12} />
              )}
              Rebuild
            </Button>
          )}
          {(state.kind === "building" || state.kind === "warming") && (
            <Button
              variant="ghost"
              size="sm"
              className="h-7 gap-1.5 px-2.5 text-xs"
              onClick={onRefresh}
            >
              <HugeiconsIcon icon={RefreshIcon} size={12} />
              Refresh
            </Button>
          )}
        </div>
      )}

      {(state.kind === "building" ||
        state.kind === "warming" ||
        state.kind === "failed") && (
        <div className="grid grid-cols-[auto_1fr] gap-x-2 gap-y-0.5 rounded-md bg-muted/30 p-2 text-[11px] text-muted-foreground">
          <span className="font-medium">Image build</span>
          <span>{pipelineLabel(state.status.image_status)}</span>
          <span className="font-medium">Graph warm</span>
          <span>{pipelineLabel(state.status.graph_warm_status)}</span>
        </div>
      )}
    </div>
  );
}

function pipelineLabel(raw: string | undefined | null): string {
  switch (raw) {
    case "none":
      return "not started";
    case "building":
      return "building…";
    case "ready":
      return "ready";
    case "failed":
      return "failed";
    case "pending":
      return "waiting for image";
    case "running":
      return "running…";
    default:
      return raw ?? "—";
  }
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
        className="inline-flex h-7 items-center gap-1 rounded-md bg-amber-500/15 px-2.5 text-xs font-medium text-amber-200 ring-1 ring-amber-500/40 transition-colors hover:bg-amber-500/25"
      >
        <HugeiconsIcon icon={GitPullRequestIcon} size={12} />
        View PR #{existingPr.number}
        <HugeiconsIcon icon={LinkSquare02Icon} size={10} />
      </a>
    );
  }

  return (
    <Button
      variant="default"
      size="sm"
      className="h-7 gap-1.5 px-2.5 text-xs"
      onClick={onOpen}
      disabled={opening}
    >
      {opening ? (
        <HugeiconsIcon
          icon={Loading02Icon}
          size={12}
          className="animate-spin"
        />
      ) : (
        <HugeiconsIcon icon={GitPullRequestIcon} size={12} />
      )}
      {opening ? "Opening PR…" : "Open PR"}
    </Button>
  );
}
