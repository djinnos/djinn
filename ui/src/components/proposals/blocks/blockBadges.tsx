// React components for the shared block visual vocabulary. The pure color logic
// (accent palette, change/method/status maps, color helpers) lives React-free in
// `blockBadges.ts`; this module only wraps it in the small reusable primitives
// (`Badge` / `Pill` / `ChangeChip`) the blocks render. This is the design-system
// seed the next blocks (diff, annotated-code, callout, table, checklist) reuse.
//
// The app is DARK-ONLY.

import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

import type { FileChange } from "./fileTree";
import {
  ACCENT_BADGE,
  CHANGE_BADGE,
  CHANGE_GLYPH,
  CHANGE_LABEL,
  type AccentColor,
} from "./blockBadgeColors";

/**
 * A small uppercase tag (the "Badge" shape). `accent` picks a palette surface;
 * pass `className` to override or extend (e.g. add `normal-case`, swap the
 * surface). When neither is given the tag is transparent (caller styles it).
 */
export function Badge({
  children,
  accent,
  className,
}: {
  children: ReactNode;
  accent?: AccentColor;
  className?: string;
}) {
  return (
    <span
      className={cn(
        "inline-flex items-center rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide leading-none",
        accent && ACCENT_BADGE[accent],
        className,
      )}
    >
      {children}
    </span>
  );
}

/** Alias of {@link Badge} — same uppercase-tag shape, read as a "pill". */
export const Pill = Badge;

/**
 * The compact A/M/D/R square chip: added emerald, modified blue, deleted red,
 * renamed violet. Carries an accessible label/title.
 */
export function ChangeChip({
  status,
  className,
}: {
  status: FileChange;
  className?: string;
}) {
  return (
    <span
      title={CHANGE_LABEL[status]}
      aria-label={CHANGE_LABEL[status]}
      className={cn(
        "flex size-4 shrink-0 items-center justify-center rounded text-[10px] font-bold leading-none",
        CHANGE_BADGE[status],
        className,
      )}
    >
      {CHANGE_GLYPH[status]}
    </span>
  );
}
