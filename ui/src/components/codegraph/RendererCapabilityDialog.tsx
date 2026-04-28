/**
 * RendererCapabilityDialog — probe WebGL2 at mount time and surface a
 * dismissible dialog when the host can't render the graph.
 *
 * Sigma 3 falls back to a software / no-render path when WebGL2 is
 * unavailable; rather than letting the canvas silently misbehave we
 * tell the user up front. The probe runs once and caches its result
 * in module scope so dialog dismissals stick across remounts within
 * the same session.
 */

import { useState } from "react";
import { HugeiconsIcon } from "@hugeicons/react";
import { AlertCircleIcon, Cancel01Icon } from "@hugeicons/core-free-icons";

let cachedProbe: "supported" | "unsupported" | null = null;
let dialogDismissed = false;

function probeWebGL2(): "supported" | "unsupported" {
  if (cachedProbe) return cachedProbe;
  if (typeof document === "undefined") {
    cachedProbe = "supported";
    return cachedProbe;
  }
  try {
    const canvas = document.createElement("canvas");
    const gl = canvas.getContext("webgl2");
    cachedProbe = gl ? "supported" : "unsupported";
  } catch {
    cachedProbe = "unsupported";
  }
  return cachedProbe;
}

export function RendererCapabilityDialog() {
  // Lazy init: probe runs once at mount via the initializer fn,
  // avoiding a setState-in-effect cascade.
  const [open, setOpen] = useState(
    () => probeWebGL2() === "unsupported" && !dialogDismissed,
  );

  if (!open) return null;

  return (
    <div
      role="alertdialog"
      aria-modal="true"
      aria-labelledby="renderer-capability-title"
      className="pointer-events-auto absolute inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm"
    >
      <div className="max-w-md rounded-xl border border-amber-500/30 bg-[#0a0a10]/95 p-5 shadow-2xl">
        <div className="flex items-start gap-3">
          <span className="flex h-10 w-10 shrink-0 items-center justify-center rounded-full bg-amber-500/15 text-amber-400">
            <HugeiconsIcon icon={AlertCircleIcon} className="h-5 w-5" />
          </span>
          <div className="min-w-0 flex-1">
            <h2
              id="renderer-capability-title"
              className="text-sm font-semibold text-zinc-100"
            >
              WebGL2 unavailable
            </h2>
            <p className="mt-1 text-xs leading-relaxed text-zinc-400">
              This browser or device doesn&apos;t expose WebGL2, which the
              code graph uses for fast canvas rendering. The page will load
              but interaction may be slow or visually degraded. Try a
              modern Chromium / Firefox build, or update your GPU drivers
              if you&apos;re on Linux.
            </p>
          </div>
          <button
            type="button"
            onClick={() => {
              dialogDismissed = true;
              setOpen(false);
            }}
            className="rounded-md p-1 text-zinc-500 transition-colors hover:bg-zinc-800/60 hover:text-zinc-200"
            aria-label="Dismiss WebGL2 warning"
          >
            <HugeiconsIcon icon={Cancel01Icon} className="h-4 w-4" />
          </button>
        </div>
        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={() => {
              dialogDismissed = true;
              setOpen(false);
            }}
            className="rounded-md border border-amber-500/30 bg-amber-500/15 px-3 py-1.5 text-xs font-medium text-amber-200 transition-colors hover:bg-amber-500/25"
          >
            Continue anyway
          </button>
        </div>
      </div>
    </div>
  );
}
