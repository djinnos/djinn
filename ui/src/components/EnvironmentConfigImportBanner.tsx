/**
 * EnvironmentConfigImportBanner — one-time nudge shown when the
 * currently-selected project's `environment_config.source` is
 * `"auto-detected"`. Clicking "Review" navigates to the editor; clicking
 * "Dismiss" hides the banner permanently (localStorage key per project).
 *
 * Scoped to a single project at a time — driven by `useSelectedProjectId`
 * so it renders inline on whatever page the user is currently looking
 * at. Stays out of the way once dismissed.
 */
import { useEffect, useState } from "react";
import { useNavigate } from "react-router-dom";
import { HugeiconsIcon } from "@hugeicons/react";
import {
  Alert02Icon,
  Cancel01Icon,
  FileValidationIcon,
} from "@hugeicons/core-free-icons";

import { Card, CardContent } from "@/components/ui/card";
import { Button } from "@/components/ui/button";
import { fetchEnvironmentConfig } from "@/api/environmentConfig";
import { useSelectedProjectId } from "@/stores/useProjectStore";

const DISMISS_PREFIX = "djinn.env-config-import-banner.dismissed:";

function dismissKey(projectId: string): string {
  return `${DISMISS_PREFIX}${projectId}`;
}

export function EnvironmentConfigImportBanner() {
  const projectId = useSelectedProjectId();
  const navigate = useNavigate();
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    let active = true;
    if (!projectId) return () => undefined;
    // Skip the network call entirely if the user dismissed for this
    // project — localStorage is the source of truth.
    let dismissed = false;
    try {
      dismissed = localStorage.getItem(dismissKey(projectId)) === "1";
    } catch {
      // localStorage unavailable (private mode, etc) — fall through; the
      // banner shows but dismissal just won't persist, which is fine.
    }
    if (dismissed) {
      return () => undefined;
    }
    void fetchEnvironmentConfig(projectId)
      .then(({ config, seeded }) => {
        if (!active) return;
        setVisible(seeded && config.source === "auto-detected");
      })
      .catch(() => {
        // Silent — the main editor page surfaces fetch failures; the
        // banner is an optional nudge.
        if (active) setVisible(false);
      });
    return () => {
      active = false;
    };
  }, [projectId]);


  if (!projectId || !visible) return null;

  const dismiss = () => {
    try {
      localStorage.setItem(dismissKey(projectId), "1");
    } catch {
      // ignored — see above
    }
    setVisible(false);
  };

  return (
    <Card className="mx-4 border-none bg-sky-500/10 ring-sky-500/40">
      <CardContent className="py-4">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-start gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-sky-500/20">
              <HugeiconsIcon icon={FileValidationIcon} className="size-4 text-sky-300" />
            </div>
            <div className="flex flex-col gap-1">
              <h3 className="text-sm font-semibold text-sky-200">
                Environment config auto-detected
              </h3>
              <p className="text-sm text-muted-foreground">
                Djinn generated this project's runtime config from the detected stack. Review it in
                the editor to confirm toolchains, workspaces, and verification rules.
              </p>
            </div>
          </div>
          <button
            type="button"
            aria-label="Dismiss auto-detection banner"
            onClick={dismiss}
            className="shrink-0 rounded-md p-1 text-muted-foreground transition-colors hover:bg-muted/40 hover:text-foreground"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="size-4" />
          </button>
        </div>

        <div className="mt-3 flex items-center gap-2 pl-11">
          <Button
            variant="outline"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={() => navigate(`/projects/${projectId}/environment`)}
          >
            <HugeiconsIcon icon={Alert02Icon} size={14} />
            Review config
          </Button>
          <Button
            variant="ghost"
            size="sm"
            className="h-7 gap-1.5 px-3 text-xs"
            onClick={dismiss}
          >
            Dismiss
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
