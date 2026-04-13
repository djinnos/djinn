export async function openUrl(url: string): Promise<void> {
  if (typeof window === "undefined") return;

  if (typeof window.electronAPI?.invoke === "function") {
    return window.electronAPI.invoke('shell:open-external', { url }) as Promise<void>;
  }

  window.open(url, "_blank", "noopener,noreferrer");
}
