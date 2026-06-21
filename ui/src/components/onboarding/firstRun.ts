import type { ModelLanes } from "@/api/userSettings";
import { stripProviderPrefix } from "@/components/userConfig/ModelSection";

/**
 * First-run onboarding sheet (slice 4 of p8py "Model setup UX").
 *
 * The sheet is a focused, sequential first-run flow shown when a user has no
 * connected providers and/or no model lanes — so they don't land on the full
 * settings page cold. It consolidates: connect a subscription → pick a working
 * style → done.
 *
 * Dismissal is deliberately CLIENT-SIDE (localStorage keyed by user id) rather
 * than a new DB column: we want to avoid adding a migration in this slice. The
 * server-side gate signals (no provider / no lanes) still govern whether the
 * sheet is eligible to show; a dismissal only suppresses it for someone who
 * chose to Skip on THIS device without finishing setup.
 */

/** localStorage key for "this user has dismissed/finished the first-run sheet". */
export function firstRunDismissKey(userId: string): string {
  return `djinn.onboarding.firstRun.dismissed.${userId}`;
}

/** Whether the given user has dismissed the first-run sheet on this device. */
export function isFirstRunDismissed(userId: string | null | undefined): boolean {
  if (!userId) return false;
  try {
    return window.localStorage.getItem(firstRunDismissKey(userId)) === "1";
  } catch {
    // Private-mode / storage-disabled: treat as not-dismissed (the server gate
    // still governs whether the sheet shows at all).
    return false;
  }
}

/** Record that the given user finished or skipped the first-run sheet. */
export function dismissFirstRun(userId: string | null | undefined): void {
  if (!userId) return;
  try {
    window.localStorage.setItem(firstRunDismissKey(userId), "1");
  } catch {
    // Best-effort: if storage is unavailable the sheet may re-appear, which is
    // acceptable — it never blocks the user (every step is skippable).
  }
}

/** Clear the dismissal flag (used by tests / a future "reset onboarding"). */
export function clearFirstRunDismissal(userId: string | null | undefined): void {
  if (!userId) return;
  try {
    window.localStorage.removeItem(firstRunDismissKey(userId));
  } catch {
    // ignore
  }
}

/**
 * One-line summary of the resulting role assignment, e.g.
 * "Plan w/ opus · Build w/ gpt-5 · Review w/ gpt-5". Uses the FIRST (primary)
 * model id in each lane, stripped of its provider prefix. Lanes with no model
 * fall back to the deployment default, surfaced as "default".
 */
export function assignmentSummary(lanes: ModelLanes): string {
  const primary = (ids: string[]): string =>
    ids.length > 0 ? stripProviderPrefix(ids[0]!) : "default";
  return [
    `Plan w/ ${primary(lanes.plan)}`,
    `Build w/ ${primary(lanes.implement)}`,
    `Review w/ ${primary(lanes.review)}`,
  ].join(" · ");
}
