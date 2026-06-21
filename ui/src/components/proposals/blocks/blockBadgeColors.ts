// Pure (React-free) half of the shared block visual vocabulary. The rewritten
// FileTree / DataModel / ApiEndpoint blocks each independently grew the same
// accent palette, A/M/D/R change colors, and HTTP-method / status-code maps.
// This module is the single source of truth for those *class strings*; the
// React components that wrap them (`Badge` / `Pill` / `ChangeChip`) live in
// `blockBadges.tsx`. Keeping the color logic React-free makes it unit-testable
// and reusable by the next blocks (diff, annotated-code, callout, table,
// checklist).
//
// The app is DARK-ONLY: accents are a tinted `bg-<color>/15` surface with a
// saturated `text-<color>-300` ink.

import type { FileChange } from "./fileTree";
import type { ApiMethod, ParamLocation, StatusClass } from "./apiEndpoint";

/* ── Accent palette ─────────────────────────────────────────────────────────── */

/** The named accents the blocks share. */
export type AccentColor =
  | "emerald"
  | "blue"
  | "amber"
  | "violet"
  | "red"
  | "slate";

/** Tinted badge surface + saturated ink, one entry per accent. Dark-only. */
export const ACCENT_BADGE: Record<AccentColor, string> = {
  emerald: "bg-emerald-500/15 text-emerald-300",
  blue: "bg-blue-500/15 text-blue-300",
  amber: "bg-amber-500/15 text-amber-300",
  violet: "bg-violet-500/15 text-violet-300",
  red: "bg-red-500/15 text-red-300",
  slate: "bg-slate-500/15 text-slate-300",
};

/* ── A/M/D/R change vocabulary ──────────────────────────────────────────────── */

/** Single-letter glyph for a change status. */
export const CHANGE_GLYPH: Record<FileChange, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
};

/** Accessible label for a change status. */
export const CHANGE_LABEL: Record<FileChange, string> = {
  added: "Added",
  modified: "Modified",
  deleted: "Deleted",
  renamed: "Renamed",
};

/** Tinted square chip background + saturated text, keyed by change status. */
export const CHANGE_BADGE: Record<FileChange, string> = {
  added: ACCENT_BADGE.emerald,
  modified: ACCENT_BADGE.blue,
  deleted: ACCENT_BADGE.red,
  renamed: ACCENT_BADGE.violet,
};

/** Accent ink for a name echoing its change color (deleted is struck through). */
export const CHANGE_NAME_INK: Record<FileChange, string> = {
  added: "text-emerald-300",
  modified: "text-blue-300",
  deleted: "text-red-300 line-through",
  renamed: "text-violet-300",
};

/* ── HTTP method + status-code + param-location colors ───────────────────────── */

/** Per-verb accent. Anything unknown falls back to neutral slate. */
const METHOD_ACCENT: Record<ApiMethod, AccentColor> = {
  GET: "emerald",
  POST: "blue",
  PUT: "amber",
  PATCH: "violet",
  DELETE: "red",
  HEAD: "slate",
  OPTIONS: "slate",
};

/** Badge classes for an HTTP verb (neutral slate for an unknown/undefined verb). */
export function httpMethodColor(method: ApiMethod | undefined): string {
  return ACCENT_BADGE[method ? METHOD_ACCENT[method] : "slate"];
}

const STATUS_ACCENT: Record<StatusClass, AccentColor> = {
  success: "emerald",
  info: "slate",
  warn: "amber",
  error: "red",
};

/** Badge classes for a status class: 2xx emerald / 4xx amber / 5xx red / slate. */
export function statusCodeColor(cls: StatusClass): string {
  return ACCENT_BADGE[STATUS_ACCENT[cls]];
}

/** IN-location accent: path violet / query blue / header amber / body emerald. */
const LOCATION_ACCENT: Record<ParamLocation, AccentColor> = {
  path: "violet",
  query: "blue",
  header: "amber",
  body: "emerald",
};

/** Badge classes for a parameter IN-location. */
export function paramLocationColor(location: ParamLocation): string {
  return ACCENT_BADGE[LOCATION_ACCENT[location]];
}
