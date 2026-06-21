// Pure (React-free) helpers for the Callout block. A callout is a tinted,
// bordered admonition card carrying a `tone` and a markdown body. The body is
// the block's *children text* (rendered through `BlockMarkdown`); `tone` is an
// optional attribute. This module only normalizes/validates the tone so the
// React component stays declarative and the mapping is unit-testable.
//
// Ported from agent-native's `callout` block (5 tones: info / decision / risk /
// warning / success). djinn maps each tone to the shared accent palette.

import type { AccentColor } from "./blockBadgeColors";

/** The five callout tones, in canonical order. */
export const CALLOUT_TONES = [
  "info",
  "decision",
  "risk",
  "warning",
  "success",
] as const;

export type CalloutTone = (typeof CALLOUT_TONES)[number];

/** The default tone when none is authored or the attribute is unrecognized. */
export const DEFAULT_CALLOUT_TONE: CalloutTone = "info";

/**
 * Normalize a raw `tone` attribute value into a known {@link CalloutTone},
 * falling back to {@link DEFAULT_CALLOUT_TONE} for missing / unknown values.
 * Case-insensitive and whitespace-tolerant so author typos degrade gracefully.
 */
export function normalizeCalloutTone(raw: string | undefined): CalloutTone {
  const value = raw?.trim().toLowerCase();
  return (CALLOUT_TONES as readonly string[]).includes(value ?? "")
    ? (value as CalloutTone)
    : DEFAULT_CALLOUT_TONE;
}

/**
 * Map a tone to a shared-palette accent: info=blue, decision=violet,
 * risk=red, warning=amber, success=emerald. Drives the border/icon tint.
 */
const TONE_ACCENT: Record<CalloutTone, AccentColor> = {
  info: "blue",
  decision: "violet",
  risk: "red",
  warning: "amber",
  success: "emerald",
};

/** The accent color for a tone. */
export function calloutToneAccent(tone: CalloutTone): AccentColor {
  return TONE_ACCENT[tone];
}
