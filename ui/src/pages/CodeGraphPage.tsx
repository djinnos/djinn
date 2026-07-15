/**
 * CodeGraphPage — top-level shell for `/code-graph`.
 *
 * lmkv cutover: the 3D galaxy (`GalaxyView`) IS the code graph. The old
 * Sigma canvas, lens toolbar, and symbol detail panel were removed with
 * it (cutover over strangler); Cmd-K hybrid search stays and flies the
 * galaxy camera to its hits via the shared codeGraphStore citation /
 * tool-highlight sets. Project selection, workspace + test filters, and
 * the hotspot panel all live inside the galaxy HUD (the chrome selector
 * is suppressed for this route — see routeScopes.ts).
 */

import { HugeiconsIcon } from "@hugeicons/react";
import { ConnectIcon } from "@hugeicons/core-free-icons";

import { GalaxyView } from "@/components/codegraph/GalaxyView";
import { QueryPalette } from "@/components/codegraph/QueryPalette";
import {
  useSelectedProject,
  useSelectedProjectId,
} from "@/stores/useProjectStore";

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
      <div className="relative flex min-h-0 flex-1">
        {project && selectedProjectId ? (
          <>
            <div className="relative min-w-0 flex-1">
              {/* `key` forces a fresh fetch + layout on project change. */}
              <GalaxyView
                key={selectedProjectId}
                projectId={selectedProjectId}
              />
            </div>
            <QueryPalette projectId={selectedProjectId} />
          </>
        ) : (
          <EmptyHint message="Select a project to view its code graph." />
        )}
      </div>
    </div>
  );
}
