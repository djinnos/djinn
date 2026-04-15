/**
 * Open an external URL in a new browser tab.
 *
 * Replaces the Electron shell.openExternal wrapper now that the
 * desktop app is a plain web client.
 */
export function openUrl(url: string): void {
  window.open(url, "_blank", "noopener,noreferrer");
}
