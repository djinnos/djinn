import {
  Alert02Icon,
  AlertDiamondIcon,
  CheckmarkCircle02Icon,
  Idea01Icon,
  InformationCircleIcon,
} from "@hugeicons/core-free-icons";
import { HugeiconsIcon } from "@hugeicons/react";

import { cn } from "@/lib/utils";

import { BlockMarkdown } from "./BlockShell";
import {
  calloutToneAccent,
  normalizeCalloutTone,
  type CalloutTone,
} from "./callout";
import type { AccentColor } from "./blockBadgeColors";
import type { BlockProps } from "./types";

/**
 * Callout — a per-tone admonition rendered as a NATIVE editorial aside: a
 * left accent bar + a very faint tone tint, NOT a full bordered grey card. The
 * `tone` attribute (info / decision / risk / warning / success, default info)
 * drives the accent via the shared palette (info=blue, decision=violet,
 * risk=red, warning=amber, success=emerald).
 *
 * The body is the block's children text, rendered through `BlockMarkdown`.
 * Ported from agent-native's `callout` block; djinn is read-only so there is no
 * tone picker / inline editor — the authored tone is rendered as-is. Dark-only.
 */
export function CalloutBlock({ attributes, children }: BlockProps) {
  const tone = normalizeCalloutTone(attributes.tone);
  const accent = calloutToneAccent(tone);
  const body = typeof children === "string" ? children : "";

  return (
    <div
      data-tone={tone}
      className={cn(
        "flex gap-3 rounded-r-md border-l-2 py-3 pl-4 pr-3",
        TONE_SURFACE[accent],
      )}
    >
      <HugeiconsIcon
        icon={TONE_ICON[tone]}
        size={18}
        className={cn("mt-0.5 shrink-0", TONE_ICON_INK[accent])}
        aria-hidden
      />
      <div className="min-w-0 flex-1">
        <div
          className={cn(
            "mb-1 text-[11px] font-semibold uppercase tracking-wider",
            TONE_ICON_INK[accent],
          )}
        >
          {TONE_LABEL[tone]}
        </div>
        <BlockMarkdown>{body}</BlockMarkdown>
      </div>
    </div>
  );
}

/** Per-tone header label (capitalized tone). */
const TONE_LABEL: Record<CalloutTone, string> = {
  info: "Info",
  decision: "Decision",
  risk: "Risk",
  warning: "Warning",
  success: "Success",
};

/** Per-tone leading icon. */
const TONE_ICON: Record<CalloutTone, typeof InformationCircleIcon> = {
  info: InformationCircleIcon,
  decision: Idea01Icon,
  risk: AlertDiamondIcon,
  warning: Alert02Icon,
  success: CheckmarkCircle02Icon,
};

/** Left accent-bar color + faint tone tint per accent (dark-only). */
const TONE_SURFACE: Record<AccentColor, string> = {
  blue: "border-blue-400/70 bg-blue-500/[0.05]",
  violet: "border-violet-400/70 bg-violet-500/[0.05]",
  red: "border-red-400/70 bg-red-500/[0.05]",
  amber: "border-amber-400/70 bg-amber-500/[0.05]",
  emerald: "border-emerald-400/70 bg-emerald-500/[0.05]",
  slate: "border-slate-400/70 bg-slate-500/[0.05]",
};

/** Saturated ink per accent for the icon + header. */
const TONE_ICON_INK: Record<AccentColor, string> = {
  blue: "text-blue-300",
  violet: "text-violet-300",
  red: "text-red-300",
  amber: "text-amber-300",
  emerald: "text-emerald-300",
  slate: "text-slate-300",
};
