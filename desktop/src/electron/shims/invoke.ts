export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (typeof window === "undefined" || typeof window.electronAPI?.invoke !== "function") {
    throw new Error(`Electron invoke '${cmd}' is unavailable in browser mode`);
  }
  if (args !== undefined) {
    return window.electronAPI.invoke(cmd, args) as Promise<T>;
  }
  return window.electronAPI.invoke(cmd) as Promise<T>;
}
