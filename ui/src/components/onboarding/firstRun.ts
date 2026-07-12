/**
 * First-run onboarding sheet.
 *
 * The sheet is a focused, sequential first-run flow shown when a user has no
 * connected providers and/or model-role assignments — so they don't land on
 * the full settings page cold. It consolidates provider connection and the
 * production Model Roles editor into one guided flow.
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
