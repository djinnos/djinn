export async function openUrl(url: string): Promise<void> {
  return window.electronAPI.invoke('shell:open-external', { url }) as Promise<void>;
}
